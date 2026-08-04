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

/// The longest lease the engine honours, whatever the envelope claims — the receiver-side mirror of
/// the host's own `RUMBLE_TTL_CEIL_MS`.
///
/// No host built from this tree can exceed it (the `PUNKTFUNK_RUMBLE_TTL_MS` hatch is clamped to
/// `[150, 5000]` before it reaches the wire), so this is defence in depth against a third-party or
/// modified sender that stamps a long TTL and then wedges its renewal pump while the connection
/// stays up. It matters on exactly the platforms that sustain a level for the whole lease: Apple,
/// whose renderer deliberately keeps no staleness policy of its own, and a Deck slot, whose
/// keepalive re-kicks the actuator until the lease ends. Duration-parameterized embedders (SDL,
/// Android) already self-terminate at the clamped backstop.
///
/// Deliberately NOT `pub`: an embedder has no use for it, and every `pub` const in this crate is
/// emitted into `include/punktfunk_core.h` as an UNPREFIXED `#define` — a collision hazard the
/// header already has ~170 instances of, and one this has no reason to add to.
const MAX_LEASE_MS: u16 = 5_000;

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
    /// output decays between wire renewals. `0` = no keepalive (the common case).
    ///
    /// The one in-tree producer is the Steam Deck's ≈ 40 ms (`pf-client-core`'s slot open, paired
    /// with `dedup_jitter`). The macOS DualSense-over-HID Bluetooth decay is NOT served by this
    /// quirk, though it reads like the obvious second example: the Apple client keeps its own
    /// ≈ 900 ms keepalive down in `RumbleRenderer` (`RumbleTuning.hidKeepaliveSeconds`) because
    /// the re-emit has to happen BELOW the command layer. An engine keepalive arrives as a
    /// command carrying the same levels, and that renderer skips a HID write whose levels are
    /// unchanged — so the re-emit would be swallowed by the very dedupe it exists to defeat
    /// (`dedup_jitter` is the Deck's answer to the same problem one layer up).
    pub keepalive_ms: u16,
    /// Floor for `backstop_ms` on non-zero commands.
    ///
    /// **No in-tree producer sets this non-zero** — it is reachable only through the C ABI
    /// (`punktfunk_connection_set_rumble_quirks`), for embedders whose duration-taking API
    /// rejects short values. The case it was written for is handled elsewhere: Android's
    /// `createOneShot` does throw on a non-positive duration, but the Kotlin renderer floors the
    /// duration itself at the call, and that path never declares quirks at all. Kept because it
    /// is exported ABI, and because a floor belongs here rather than re-invented per embedder.
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
    /// The exact value last handed to an embedder. `(0, 0)` ⇔ the engine believes this actuator is
    /// silent. It replaces a free-running jitter phase because one field answers all three live
    /// questions: would re-sending this be a no-op device write (the dedupe nudge), is a stop
    /// redundant, and would the nudge synthesize the reserved stop.
    last_emit: (u16, u16),
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
        last_emit: (0, 0),
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
        self.last_emit = (0, 0);
        RumbleCommand {
            pad,
            low: 0,
            high: 0,
            backstop_ms: 0,
        }
    }

    /// Build the command for the pad's current level, and record what we handed out.
    ///
    /// On a `dedup_jitter` actuator, re-emitting the value the device last took is a no-op write on
    /// an SDL-class layer, so the low motor's LSB is nudged. Keying that on `last_emit` rather than
    /// on a free-running phase is what makes it work on EVERY emit path. Previously the nudge lived
    /// only in the keepalive branch, so a host renewal — which arrives every `ttl*3/10` ms, 120 ms
    /// at the 400 ms default and 60 ms at the hatch floor — re-emitted the raw level, collided with
    /// the last jittered write, was swallowed, AND re-anchored the keepalive. That stretched the
    /// gap between *distinct* device writes to 80 ms at the default cadence and 100 ms at the
    /// floor, on an actuator whose quirk declares 40.
    ///
    /// The nudge is refused when it would synthesize the reserved `(0, 0)` stop. That is level
    /// `(1, 0)` and only that: `high` must already be 0, and `low ^ 1 == 0` implies `low == 1`.
    /// There the LSB steps up instead, so the phase still alternates (1 ↔ 3, two parts in 65535)
    /// and the pad never receives a stop the policy did not order.
    fn emit(&mut self, pad: u16) -> RumbleCommand {
        let (mut low, high) = self.level;
        if self.quirks.dedup_jitter && (low, high) == self.last_emit {
            let alt = low ^ 1;
            low = if (alt, high) == (0, 0) {
                low | 0b10
            } else {
                alt
            };
        }
        self.last_emit = (low, high);
        RumbleCommand {
            pad,
            low,
            high,
            backstop_ms: self.backstop(),
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
                // Never honour a lease longer than [`MAX_LEASE_MS`], whatever the sender claims.
                let t = t.min(MAX_LEASE_MS);
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
                    // Relay a stop only if the actuator is, as far as the engine knows, still
                    // buzzing. A zero on an already-silent pad heals nothing and costs every
                    // embedder a command — Android an unconditional log line plus a binder
                    // `cancel()`. Two senders produce them: the host's deliberate
                    // `RUMBLE_STOP_BURST` re-sends after the first stop already landed, and (behind
                    // `PUNKTFUNK_RUMBLE_ENVELOPE=0`) the legacy flat 500 ms refresh, which re-sends
                    // zeros for every latched pad for the rest of the session. The burst still
                    // heals the case it exists for: a LOST first stop leaves the pad buzzing, so
                    // `last_emit != (0, 0)` and the re-send does emit.
                    if p.last_emit != (0, 0) {
                        return (Some(p.silence(pad)), None);
                    }
                    continue;
                }
                if p.quirks.keepalive_ms > 0 {
                    p.next_keepalive =
                        Some(now + Duration::from_millis(p.quirks.keepalive_ms as u64));
                }
                return (Some(p.emit(pad)), None);
            }
            // 4) actuator-decay keepalive, bounded by (1)/(2) above by construction: an expired
            // or stale pad was silenced before reaching here, so a keepalive can never sustain a
            // level the policy has ended.
            if p.level != (0, 0) && p.quirks.keepalive_ms > 0 {
                let ka = Duration::from_millis(p.quirks.keepalive_ms as u64);
                let due = *p.next_keepalive.get_or_insert(now + ka);
                if now >= due {
                    p.next_keepalive = Some(now + ka);
                    return (Some(p.emit(pad)), None);
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

    /// The Steam Deck's declared quirks — the only shipping actuator with `dedup_jitter`.
    const DECK: ActuatorQuirks = ActuatorQuirks {
        keepalive_ms: 40,
        min_pulse_ms: 0,
        dedup_jitter: true,
    };

    /// Drain the engine the way an embedder does: poll until nothing is due.
    fn drain(e: &mut RumbleEngine, t: Instant) -> Vec<(u16, u16)> {
        let mut out = Vec::new();
        while let (Some(c), _) = e.poll(t) {
            out.push((c.low, c.high));
        }
        out
    }

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

    /// A host renewal must not repeat the value the device last took, or an SDL-class layer
    /// swallows the write. Before the jitter moved onto every emit path it lived only in the
    /// keepalive branch, so each renewal collided with the last jittered write and was deduped.
    #[test]
    fn renewal_keeps_the_dedupe_jitter_alternating() {
        let mut e = RumbleEngine::new();
        e.set_quirks(0, DECK);
        let t0 = Instant::now();
        e.wire_update(t0, 0, 100, 200, Some(400));
        assert_eq!(drain(&mut e, t0), vec![(100, 200)]);
        assert_eq!(drain(&mut e, t0 + ms(40)), vec![(101, 200)]);
        assert_eq!(drain(&mut e, t0 + ms(80)), vec![(100, 200)]);
        // The renewal at the 120 ms default cadence: same level, must still be a distinct write.
        e.wire_update(t0 + ms(120), 0, 100, 200, Some(400));
        assert_eq!(drain(&mut e, t0 + ms(120)), vec![(101, 200)]);
        assert_eq!(drain(&mut e, t0 + ms(160)), vec![(100, 200)]);
    }

    /// Phase-robust version of the same property, at the TTL hatch's 60 ms renewal floor: no two
    /// consecutive DISTINCT device writes may be further apart than the declared 40 ms cadence.
    #[test]
    fn renewal_never_gaps_distinct_writes_at_the_60ms_floor() {
        let mut e = RumbleEngine::new();
        e.set_quirks(0, DECK);
        let t0 = Instant::now();
        let (mut last, mut last_write, mut worst) = ((0u16, 0u16), 0u64, 0u64);
        for tick in 0..=360u64 {
            let t = t0 + ms(tick);
            if tick % 60 == 0 {
                e.wire_update(t, 0, 100, 200, Some(400));
            }
            for v in drain(&mut e, t) {
                assert_ne!(v, (0, 0), "a live lease must never emit the stop sentinel");
                if v != last {
                    worst = worst.max(tick - last_write);
                    last_write = tick;
                    last = v;
                }
            }
        }
        assert!(
            worst <= 41,
            "worst distinct-write gap {worst} ms exceeds the 40 ms declared cadence"
        );
    }

    /// The nudge must stay behind `dedup_jitter`: an off-by-one amplitude on a default-quirks pad
    /// would land in Apple's identical-target comparison and Android's one-shot amplitudes.
    #[test]
    fn default_quirks_pads_get_the_level_verbatim_on_every_renewal() {
        let mut e = RumbleEngine::new(); // Apple / Android / plain SDL
        let t0 = Instant::now();
        e.wire_update(t0, 0, 100, 200, Some(400));
        assert_eq!(e.poll(t0).0, Some(cmd(0, 100, 200, 800)));
        e.wire_update(t0 + ms(120), 0, 100, 200, Some(400));
        assert_eq!(e.poll(t0 + ms(120)).0, Some(cmd(0, 100, 200, 800)));
    }

    /// Level `(1, 0)` is the one value whose LSB flip is the reserved stop. The nudge steps up
    /// instead, so the phase still alternates and no stop is invented under a live lease.
    #[test]
    fn jitter_never_synthesizes_the_stop_sentinel() {
        let mut e = RumbleEngine::new();
        e.set_quirks(0, DECK);
        let t0 = Instant::now();
        e.wire_update(t0, 0, 1, 0, Some(400));
        assert_eq!(e.poll(t0).0, Some(cmd(0, 1, 0, 800)));
        assert_eq!(e.poll(t0 + ms(40)).0, Some(cmd(0, 3, 0, 800)));
        assert_eq!(e.poll(t0 + ms(80)).0, Some(cmd(0, 1, 0, 800)));
    }

    /// A zero for a pad the engine already believes is silent is dropped: it heals nothing and
    /// costs every embedder a command. The deliberate stop-burst heal is unaffected, because a
    /// LOST stop leaves the pad buzzing and the re-send therefore does emit.
    #[test]
    fn a_redundant_stop_is_dropped_but_the_burst_still_heals_a_lost_one() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        e.wire_update(t0, 0, 100, 200, Some(400));
        assert_eq!(drain(&mut e, t0), vec![(100, 200)]);
        // First stop reaches the embedder…
        e.wire_update(t0 + ms(10), 0, 0, 0, Some(0));
        assert_eq!(drain(&mut e, t0 + ms(10)), vec![(0, 0)]);
        // …and the burst re-sends behind it are now silent.
        e.wire_update(t0 + ms(20), 0, 0, 0, Some(0));
        e.wire_update(t0 + ms(30), 0, 0, 0, Some(0));
        assert_eq!(drain(&mut e, t0 + ms(30)), Vec::new());

        // But if the pad is buzzing (the stop that mattered was lost), a re-send still emits.
        e.wire_update(t0 + ms(40), 0, 100, 200, Some(400));
        assert_eq!(drain(&mut e, t0 + ms(40)), vec![(100, 200)]);
        e.wire_update(t0 + ms(50), 0, 0, 0, Some(0));
        assert_eq!(drain(&mut e, t0 + ms(50)), vec![(0, 0)]);
    }

    /// The client bounds the host's lease. `RUMBLE_TTL_CEIL_MS` is sender-side only, so a modified
    /// or third-party host could otherwise stamp a huge TTL and wedge its pump, leaving Apple and
    /// the Deck buzzing for the whole of it.
    #[test]
    fn an_overlong_lease_is_clamped_to_the_ceiling() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        e.wire_update(t0, 0, 100, 200, Some(u16::MAX));
        assert_eq!(e.poll(t0).0, Some(cmd(0, 100, 200, 5000)));
        // Silenced at the ceiling, not at the 65 s the sender asked for.
        assert!(e.poll(t0 + ms(MAX_LEASE_MS as u64 - 1)).0.is_none());
        assert_eq!(
            e.poll(t0 + ms(MAX_LEASE_MS as u64)).0,
            Some(cmd(0, 0, 0, 0)),
            "the lease must end at the ceiling"
        );
    }

    /// A v2 envelope carrying `ttl_ms == 0` on a LIVE level. The audit suspected the zero would be
    /// mistaken for the legacy sentinel in `backstop()`; it cannot, because the expiry check
    /// preempts the relay branch — the pad silences on the same poll and never reaches a backstop.
    /// Pinned so that ordering stays load-bearing rather than incidental.
    #[test]
    fn a_zero_ttl_envelope_silences_rather_than_taking_the_legacy_backstop() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        e.wire_update(t0, 0, 100, 200, Some(0));
        assert_eq!(
            e.poll(t0).0,
            Some(cmd(0, 0, 0, 0)),
            "a zero-length lease must expire immediately, not emit with a legacy backstop"
        );
    }
}
