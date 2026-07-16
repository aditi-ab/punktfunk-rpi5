//! The pending-approval queue + delegated approval (roadmap §8b-1; plan §W5 — carved out of the
//! [`super`] facade). Owns the pending-knock [`Mutex`] and the change [`Notify`] that wakes a QUIC
//! connection parked in [`ApprovalQueue::wait_for_decision`] the instant an operator acts.
//!
//! This module is deliberately blind to the trust store: whether a fingerprint became paired is
//! injected into [`ApprovalQueue::wait_for_decision`] as an `is_paired` closure, and the store side
//! of admitting a device (persisting the pairing) is the facade's job — this queue only records the
//! admitted knock generation and clears the entry ([`ApprovalQueue::admit_and_clear`]).

use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// An unpaired (but identified) device that knocked on a pairing-required host — held for
/// **delegated approval** from the management console (roadmap §8b-1) instead of being silently
/// forgotten. In-memory only: pending knocks don't survive a restart (the device just knocks
/// again), and they expire after [`PENDING_TTL`].
struct Pending {
    id: u32,
    name: String,
    fp_hex: String,
    requested_at: Instant,
    /// QUIC-validated source address of the knock — used for the per-source cap (#13), so one host
    /// can't fill the queue. `None` if unknown (e.g. tests / a caller that doesn't supply it).
    src_ip: Option<IpAddr>,
    /// True while a connection is held open in [`ApprovalQueue::wait_for_decision`] for this knock.
    /// A live parked knock is a genuine device waiting for the operator — eviction skips it unless
    /// every entry is parked, so a cert-rotating flood can't evict the device being onboarded (#13).
    parked: bool,
    /// Generation of the MOST RECENT knock for this fingerprint. A re-knock bumps it (and wakes
    /// waiters), so a stale parked connection resolves [`PairingDecision::Superseded`] instead of
    /// being admitted alongside the newest one — one Approve must admit exactly ONE session.
    /// (Observed live: a client retried 3× while parked, one console Approve admitted all three,
    /// and the three concurrent Mutter virtual monitors segfaulted gnome-shell.)
    knock_seq: u32,
}

#[derive(Default)]
struct PendingState {
    next_id: u32,
    items: Vec<Pending>,
    /// Fingerprint → the knock generation an approval admitted, kept briefly after
    /// [`ApprovalQueue::admit_and_clear`] clears the pending entry. Closes the last double-admit
    /// window: a superseded waiter that only polls AFTER the approval (entry gone, fingerprint
    /// paired) can't tell it lost from the entry alone — this marker lets it resolve `Superseded`
    /// instead of a second `Approved`. Pruned on the pending TTL and overwritten per fingerprint, so
    /// it stays a handful of tuples.
    admitted: Vec<(String, u32, Instant)>,
}

/// A pending-approval snapshot for the management API / web console.
pub struct PendingRequest {
    /// Per-process id used to address approve/deny (stable for the entry's lifetime).
    pub id: u32,
    /// Best-effort device label (the client's `Hello` name, else fingerprint-derived).
    pub name: String,
    /// Hex SHA-256 of the knocking client's certificate — what approval pins.
    pub fingerprint: String,
    /// Seconds since the (most recent) knock.
    pub age_secs: u64,
}

/// The outcome of a `wait_for_decision` park — what an operator did with a parked,
/// unpaired knock (delegated approval, roadmap §8b-1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairingDecision {
    /// The operator clicked Approve (the fingerprint is now paired) — admit the session.
    Approved,
    /// The operator denied, or the pending entry was otherwise dropped without pairing — reject.
    Denied,
    /// No decision within the wait window — reject; the device can knock again.
    TimedOut,
    /// A NEWER knock from the same fingerprint replaced this one — close this connection; the
    /// newest parked connection is the one an approval admits (a retrying client abandons its
    /// older attempts, and admitting them all crashes compositors — see [`Pending::knock_seq`]).
    Superseded,
}

/// Pending knocks older than this are dropped (the device retries; a stale entry shouldn't be
/// approvable days later when the operator no longer remembers the context).
const PENDING_TTL: Duration = Duration::from_secs(10 * 60);
/// Cap on the pending list — a LAN scanner must not grow it unboundedly. Oldest entries drop.
/// (`pub(super)` so the facade's behavior tests can assert the cap holds.)
pub(super) const PENDING_CAP: usize = 32;
/// Max pending knocks one source IP may occupy, so a single host can't fill the whole queue and hide
/// / evict a genuine device's knock (security-review 2026-06-28 #13). The QUIC path is address-
/// validated, so the source IP isn't off-path spoofable; an attacker would need that many real hosts.
/// (`pub(super)` so the facade's behavior tests can assert the per-IP cap holds.)
pub(super) const MAX_PENDING_PER_IP: usize = 4;

/// The pending-approval queue: the pending-knock list behind a [`Mutex`], plus the [`Notify`] that
/// wakes parked waiters when the trust/pending state changes.
pub(super) struct ApprovalQueue {
    pending: Mutex<PendingState>,
    /// Notified whenever the trust/pending state changes (a fingerprint paired, or a pending knock
    /// denied/dropped), so a QUIC connection parked in [`ApprovalQueue::wait_for_decision`] wakes
    /// the instant an operator acts in the console — the substrate for delegated approval admitting
    /// a session with no client reconnect.
    changed: Notify,
}

impl ApprovalQueue {
    pub(super) fn new() -> ApprovalQueue {
        ApprovalQueue {
            pending: Mutex::new(PendingState::default()),
            changed: Notify::new(),
        }
    }

    /// Drop expired pending knocks (called under the lock, mirroring the arming expiry). The
    /// admitted-generation markers share the TTL — they only matter while a superseded waiter
    /// could still be parked, which is bounded by the approval wait (well under the TTL).
    fn expire_pending(pending: &mut PendingState) {
        pending
            .items
            .retain(|p| p.requested_at.elapsed() < PENDING_TTL);
        pending
            .admitted
            .retain(|(_, _, at)| at.elapsed() < PENDING_TTL);
    }

    /// Pick the entry to evict, optionally restricted to a single source IP: the least-recently-active
    /// **non-parked** entry (a live parked knock is a genuine device awaiting the operator — never
    /// evict it under load); only if every candidate is parked does it fall back to the oldest of
    /// those (#13). Returns the index, or `None` if there's nothing to evict.
    fn evict_index(items: &[Pending], only_ip: Option<IpAddr>) -> Option<usize> {
        let pick = |allow_parked: bool| {
            items
                .iter()
                .enumerate()
                .filter(|(_, p)| only_ip.is_none_or(|ip| p.src_ip == Some(ip)))
                .filter(|(_, p)| allow_parked || !p.parked)
                .min_by_key(|(_, p)| p.requested_at)
                .map(|(i, _)| i)
        };
        pick(false).or_else(|| pick(true))
    }

    /// Record an unpaired device's knock for delegated approval. Re-knocks from the same fingerprint
    /// refresh the existing entry in place (same id; a connect-retry loop must not spam the list) and
    /// bump its knock generation — the returned generation is what [`Self::wait_for_decision`] admits,
    /// so the NEWEST connection wins and any older parked sibling resolves `Superseded`. A
    /// fresh fingerprint gets a new id; the queue is bounded two ways so a flood can't crowd out a
    /// genuine knock (#13): a **per-source-IP cap** ([`MAX_PENDING_PER_IP`]) means one host can hold at
    /// most a few slots, and the global [`PENDING_CAP`] evicts the least-recently-active **non-parked**
    /// entry (never a live, held-open parked knock). The name is sanitized (untrusted).
    pub(super) fn note_pending(&self, name: &str, fp_hex: &str, src_ip: Option<IpAddr>) -> u32 {
        let name = super::sanitize_device_name(name, fp_hex);
        let mut pending = self.pending.lock().unwrap();
        Self::expire_pending(&mut pending);
        if let Some(p) = pending
            .items
            .iter_mut()
            .find(|p| p.fp_hex.eq_ignore_ascii_case(fp_hex))
        {
            p.requested_at = Instant::now();
            p.name = name;
            if p.src_ip.is_none() {
                p.src_ip = src_ip;
            }
            p.knock_seq = p.knock_seq.wrapping_add(1);
            let seq = p.knock_seq;
            drop(pending);
            // Wake the previous knock's parked waiter so it sees it was superseded NOW instead of
            // holding its dead connection open until the approval window lapses.
            self.changed.notify_waiters();
            return seq;
        }
        // A fresh knock lifecycle: drop any admitted-generation marker left from a previous
        // pair→unpair round of this fingerprint, or it would wrongly supersede the new waiter.
        pending
            .admitted
            .retain(|(fp, _, _)| !fp.eq_ignore_ascii_case(fp_hex));
        // Per-source-IP cap: a single host can't occupy more than MAX_PENDING_PER_IP slots — evict its
        // own oldest entry first so it can't crowd out other devices' knocks (#13).
        if let Some(ip) = src_ip {
            if pending
                .items
                .iter()
                .filter(|p| p.src_ip == Some(ip))
                .count()
                >= MAX_PENDING_PER_IP
            {
                if let Some(i) = Self::evict_index(&pending.items, Some(ip)) {
                    pending.items.remove(i);
                }
            }
        }
        // Global cap: evict the least-recently-active non-parked entry (Vec order no longer tracks
        // recency after in-place refreshes, so pick explicitly).
        if pending.items.len() >= PENDING_CAP {
            if let Some(i) = Self::evict_index(&pending.items, None) {
                pending.items.remove(i);
            }
        }
        let id = pending.next_id;
        pending.next_id = pending.next_id.wrapping_add(1);
        pending.items.push(Pending {
            id,
            name,
            fp_hex: fp_hex.to_string(),
            requested_at: Instant::now(),
            src_ip,
            parked: false,
            knock_seq: 0,
        });
        0
    }

    /// Mark/unmark the pending entry for `fp_hex` as having a live parked waiter (no-op if it's gone).
    /// A parked entry is protected from eviction under load (#13). Gated on `knock_seq` so a
    /// superseded waiter's exit can't unmark the flag the NEWER waiter (a bumped generation) owns.
    pub(super) fn set_parked(&self, fp_hex: &str, knock_seq: u32, parked: bool) {
        let mut pending = self.pending.lock().unwrap();
        if let Some(p) = pending
            .items
            .iter_mut()
            .find(|p| p.fp_hex.eq_ignore_ascii_case(fp_hex) && p.knock_seq == knock_seq)
        {
            p.parked = parked;
        }
    }

    /// The current knock generation for `fp_hex`, `None` when no entry is pending. A parked waiter
    /// compares this against its own generation to detect it was superseded by a re-knock.
    fn knock_seq_of(&self, fp_hex: &str) -> Option<u32> {
        let pending = self.pending.lock().unwrap();
        pending
            .items
            .iter()
            .find(|p| p.fp_hex.eq_ignore_ascii_case(fp_hex))
            .map(|p| p.knock_seq)
    }

    /// The knock generation the approval of `fp_hex` admitted, if one was recorded (see
    /// [`PendingState::admitted`]).
    fn admitted_seq(&self, fp_hex: &str) -> Option<u32> {
        let pending = self.pending.lock().unwrap();
        pending
            .admitted
            .iter()
            .find(|(fp, _, _)| fp.eq_ignore_ascii_case(fp_hex))
            .map(|(_, seq, _)| *seq)
    }

    /// The devices currently awaiting approval (for the management API).
    pub(super) fn pending(&self) -> Vec<PendingRequest> {
        let mut pending = self.pending.lock().unwrap();
        Self::expire_pending(&mut pending);
        pending
            .items
            .iter()
            .map(|p| PendingRequest {
                id: p.id,
                name: p.name.clone(),
                fingerprint: p.fp_hex.clone(),
                age_secs: p.requested_at.elapsed().as_secs(),
            })
            .collect()
    }

    /// Is a knock for this fingerprint still awaiting approval? (Expired entries are dropped
    /// first, so this also reports whether a parked knock is still live.)
    pub(super) fn pending_contains(&self, fp_hex: &str) -> bool {
        let mut pending = self.pending.lock().unwrap();
        Self::expire_pending(&mut pending);
        pending
            .items
            .iter()
            .any(|p| p.fp_hex.eq_ignore_ascii_case(fp_hex))
    }

    /// Read (do NOT remove) the `(name, fingerprint)` of the pending entry with `id`, expiring
    /// stale entries first. `None` = no such (or expired) id. The facade approves by reading here,
    /// pairing the fingerprint in the trust store, and THEN [`Self::admit_and_clear`]-ing — an order
    /// [`Self::wait_for_decision`] relies on so a parked waiter never observes the device as
    /// "neither pending nor paired" (which would read as a denial). Removing here first would open
    /// exactly that window.
    pub(super) fn read_entry(&self, id: u32) -> Option<(String, String)> {
        let mut pending = self.pending.lock().unwrap();
        Self::expire_pending(&mut pending);
        pending
            .items
            .iter()
            .find(|p| p.id == id)
            .map(|p| (p.name.clone(), p.fp_hex.clone()))
    }

    /// The pending side of admitting a now-paired device: record WHICH knock generation this pairing
    /// admits (so only the waiter holding that generation returns `Approved`; a superseded sibling
    /// that polls after the clear resolves `Superseded` off this marker — exactly-one-admission, see
    /// [`PendingState::admitted`]), clear the entry, then wake parked waiters. The caller MUST have
    /// pinned the fingerprint in the trust store FIRST, so a woken waiter observes the fully settled
    /// state (paired = true, no longer pending) — see [`Self::wait_for_decision`].
    pub(super) fn admit_and_clear(&self, fp_hex: &str) {
        {
            let mut pending = self.pending.lock().unwrap();
            let admitted_seq = pending
                .items
                .iter()
                .find(|p| p.fp_hex.eq_ignore_ascii_case(fp_hex))
                .map(|p| p.knock_seq);
            if let Some(seq) = admitted_seq {
                pending
                    .admitted
                    .retain(|(fp, _, _)| !fp.eq_ignore_ascii_case(fp_hex));
                pending
                    .admitted
                    .push((fp_hex.to_string(), seq, Instant::now()));
            }
            pending
                .items
                .retain(|p| !p.fp_hex.eq_ignore_ascii_case(fp_hex));
        }
        // Wake any connection parked in `wait_for_decision` for this fingerprint: pairing just
        // completed (console approve or the PIN ceremony), so it can admit the session with no
        // reconnect. Notified AFTER the pin AND the pending-clear so a woken waiter observes the
        // fully settled state — see `wait_for_decision`.
        self.changed.notify_waiters();
    }

    /// Deny (drop) a pending knock. Returns whether one was removed. The device's next knock
    /// re-creates an entry — deny is "not now", not a blocklist.
    pub(super) fn deny_pending(&self, id: u32) -> bool {
        let removed = {
            let mut pending = self.pending.lock().unwrap();
            let before = pending.items.len();
            pending.items.retain(|p| p.id != id);
            pending.items.len() != before
        };
        if removed {
            // Wake a parked waiter so it returns `Denied` at once instead of holding the
            // connection open until the approval window lapses.
            self.changed.notify_waiters();
        }
        removed
    }

    /// Park (async) until an operator decides on a knock identified by `fp_hex`, up to `timeout`.
    /// `knock_seq` is the generation [`Self::note_pending`] returned for THIS connection's knock;
    /// `is_paired` is injected by the facade (this queue is blind to the trust store). Returns
    /// [`PairingDecision::Approved`] the instant the fingerprint is paired (console approve or a
    /// concurrent PIN ceremony), [`PairingDecision::Superseded`] the instant a newer knock from the
    /// same fingerprint replaces this one (a retrying client — only the newest connection is
    /// admitted; three siblings admitted at once has crashed gnome-shell live),
    /// [`PairingDecision::Denied`] if its pending entry is dropped without pairing, or
    /// [`PairingDecision::TimedOut`] if the window lapses. Holds no lock across the await. The
    /// QUIC accept path calls this right after [`Self::note_pending`] to keep the knocking
    /// connection open until a human clicks Approve — so the device pairs and streams with no
    /// reconnect (delegated approval, roadmap §8b-1).
    pub(super) async fn wait_for_decision(
        &self,
        fp_hex: &str,
        knock_seq: u32,
        timeout: Duration,
        is_paired: impl Fn(&str) -> bool,
    ) -> PairingDecision {
        // Mark this knock parked so a cert-rotating flood can't evict the genuine, held-open
        // connection out of the pending queue while the operator decides (#13). Cleared on every
        // exit path by the guard's Drop (generation-gated, so a superseded waiter's exit never
        // unmarks the newer waiter's flag).
        self.set_parked(fp_hex, knock_seq, true);
        struct ParkGuard<'a> {
            q: &'a ApprovalQueue,
            fp: &'a str,
            seq: u32,
        }
        impl Drop for ParkGuard<'_> {
            fn drop(&mut self) {
                self.q.set_parked(self.fp, self.seq, false);
            }
        }
        let _park = ParkGuard {
            q: self,
            fp: fp_hex,
            seq: knock_seq,
        };
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Arm the wakeup BEFORE re-reading state, and `enable()` it, so an approve/deny that
            // lands between the state check and the await still wakes us (no lost notification).
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            // Superseded check FIRST: once a newer knock owns the fingerprint, this connection
            // must never be admitted — not even if the approval lands before we wake.
            match self.knock_seq_of(fp_hex) {
                Some(cur) if cur != knock_seq => return PairingDecision::Superseded,
                _ => {}
            }
            if is_paired(fp_hex) {
                // Paired with the pending entry already cleared: make sure the approval admitted
                // OUR generation. A superseded waiter that first polls after the pairing sees the
                // same paired/no-entry state as the winner — the admitted marker breaks the tie.
                match self.admitted_seq(fp_hex) {
                    Some(adm) if adm != knock_seq => return PairingDecision::Superseded,
                    _ => return PairingDecision::Approved,
                }
            }
            if !self.pending_contains(fp_hex) {
                // Neither pending nor paired. This is almost always a denial — but it can also be
                // the tiny interval inside the facade's `add()` between pinning and clearing the
                // pending entry. Re-check `is_paired` once: because the facade pins BEFORE it clears
                // pending, a cleared-pending observation that is really an approval will now read as
                // paired — with the same generation tie-break as above (the admitted marker is
                // written in the same critical section that clears the entry).
                if is_paired(fp_hex) {
                    match self.admitted_seq(fp_hex) {
                        Some(adm) if adm != knock_seq => return PairingDecision::Superseded,
                        _ => return PairingDecision::Approved,
                    }
                }
                return PairingDecision::Denied;
            }

            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep_until(deadline) => return PairingDecision::TimedOut,
            }
        }
    }
}
