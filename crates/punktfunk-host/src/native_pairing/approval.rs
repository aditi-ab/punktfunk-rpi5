//! Pending-knock queue and delegated operator approval.
//!
//! Owns the pending-knock [`Mutex`] and the change [`Notify`] that wakes a QUIC
//! connection parked in [`ApprovalQueue::wait_for_decision`] when an operator
//! acts.
//!
//! Blind to the trust store: whether a fingerprint is paired is injected into
//! [`ApprovalQueue::wait_for_decision`] as an `is_paired` closure. The facade
//! persists the pairing; this queue records the admitted knock generation and
//! clears the entry ([`ApprovalQueue::admit_and_clear`]).

use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// Unpaired identified knock held for console approval.
/// In-memory only: a restart drops it and the device knocks again; entries
/// expire after [`PENDING_TTL`].
struct Pending {
    id: u32,
    name: String,
    fp_hex: String,
    requested_at: Instant,
    /// QUIC-validated knock source, used for the per-source cap. `None` if unknown.
    src_ip: Option<IpAddr>,
    /// True while [`ApprovalQueue::wait_for_decision`] holds this knock open.
    /// Eviction skips a parked entry unless every candidate is parked.
    parked: bool,
    /// Generation of the most recent knock for this fingerprint. A re-knock
    /// bumps it so a stale parked waiter resolves [`PairingDecision::Superseded`]
    /// — one Approve admits exactly one session.
    knock_seq: u32,
}

#[derive(Default)]
struct PendingState {
    next_id: u32,
    items: Vec<Pending>,
    /// Fingerprint → admitted knock generation, kept after
    /// [`ApprovalQueue::admit_and_clear`] clears the pending entry. A superseded
    /// waiter that polls after the entry is gone uses this to resolve `Superseded`
    /// instead of a second `Approved`. Pruned on the pending TTL.
    admitted: Vec<(String, u32, Instant)>,
}

/// Pending-approval snapshot for the management API.
pub struct PendingRequest {
    /// Per-process id for approve/deny; stable for this entry's lifetime.
    pub id: u32,
    /// Client `Hello` name, or fingerprint-derived if missing.
    pub name: String,
    /// Hex SHA-256 of the knocking client's certificate; approval pins this.
    pub fingerprint: String,
    pub age_secs: u64,
}

/// Outcome of a `wait_for_decision` park on an unpaired knock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairingDecision {
    /// Fingerprint is now paired; admit the session.
    Approved,
    /// Operator denied, or the pending entry dropped without pairing.
    Denied,
    /// Wait window elapsed; the device can knock again.
    TimedOut,
    /// A newer knock from the same fingerprint replaced this one; close this
    /// connection. Approval admits only the newest parked waiter.
    Superseded,
}

/// Drop pending knocks older than this; a stale entry must not stay approvable.
const PENDING_TTL: Duration = Duration::from_secs(10 * 60);
/// Hard cap on the pending list so a LAN scanner cannot grow it unboundedly.
pub(super) const PENDING_CAP: usize = 32;
/// Max pending knocks one source IP may occupy so one host cannot fill the queue.
/// QUIC address-validates the source, so this is not off-path spoofable.
pub(super) const MAX_PENDING_PER_IP: usize = 4;

pub(super) struct ApprovalQueue {
    pending: Mutex<PendingState>,
    /// Fired when a fingerprint is paired or a pending knock is denied/dropped.
    changed: Notify,
}

impl ApprovalQueue {
    pub(super) fn new() -> ApprovalQueue {
        ApprovalQueue {
            pending: Mutex::new(PendingState::default()),
            changed: Notify::new(),
        }
    }

    /// Admitted-generation markers share the pending TTL; they only matter while
    /// a superseded waiter could still be parked.
    fn expire_pending(pending: &mut PendingState) {
        pending
            .items
            .retain(|p| p.requested_at.elapsed() < PENDING_TTL);
        pending
            .admitted
            .retain(|(_, _, at)| at.elapsed() < PENDING_TTL);
    }

    /// Index of the entry to evict, optionally restricted to one source IP:
    /// least-recently-active non-parked, else oldest parked. `None` if empty.
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

    /// Record an unpaired knock. Same fingerprint refreshes in place and bumps
    /// generation so older parked waiters resolve `Superseded`. Bounded by
    /// [`MAX_PENDING_PER_IP`] then [`PENDING_CAP`]; the name is untrusted.
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
            // Wake the previous parked waiter so it sees Superseded now, not at timeout.
            self.changed.notify_waiters();
            return seq;
        }
        // Drop a leftover admitted-generation marker from a prior pair→unpair of this fp.
        pending
            .admitted
            .retain(|(fp, _, _)| !fp.eq_ignore_ascii_case(fp_hex));
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
        // Vec order is not recency after in-place refreshes; pick explicitly.
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

    /// Gated on `knock_seq` so a superseded waiter's Drop cannot unmark the newer waiter.
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

    fn knock_seq_of(&self, fp_hex: &str) -> Option<u32> {
        let pending = self.pending.lock().unwrap();
        pending
            .items
            .iter()
            .find(|p| p.fp_hex.eq_ignore_ascii_case(fp_hex))
            .map(|p| p.knock_seq)
    }

    fn admitted_seq(&self, fp_hex: &str) -> Option<u32> {
        let pending = self.pending.lock().unwrap();
        pending
            .admitted
            .iter()
            .find(|(fp, _, _)| fp.eq_ignore_ascii_case(fp_hex))
            .map(|(_, seq, _)| *seq)
    }

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

    /// Expires stale entries first, so this also reports a parked knock as live.
    pub(super) fn pending_contains(&self, fp_hex: &str) -> bool {
        let mut pending = self.pending.lock().unwrap();
        Self::expire_pending(&mut pending);
        pending
            .items
            .iter()
            .any(|p| p.fp_hex.eq_ignore_ascii_case(fp_hex))
    }

    /// `(name, fingerprint)` of pending `id` without removing it. `None` if missing
    /// or expired. The facade must pair in the trust store before
    /// [`Self::admit_and_clear`]; removing here first would let a waiter observe
    /// "neither pending nor paired" and treat approval as denial.
    pub(super) fn read_entry(&self, id: u32) -> Option<(String, String)> {
        let mut pending = self.pending.lock().unwrap();
        Self::expire_pending(&mut pending);
        pending
            .items
            .iter()
            .find(|p| p.id == id)
            .map(|p| (p.name.clone(), p.fp_hex.clone()))
    }

    /// Record which knock generation this pairing admits, clear the pending entry,
    /// then wake waiters. The caller must have pinned the fingerprint in the
    /// trust store first so a woken waiter sees paired=true and no longer pending.
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
        // After pin + pending-clear so a waiter observes the settled state.
        self.changed.notify_waiters();
    }

    /// Drop a pending knock. The next knock re-creates an entry — not a blocklist.
    pub(super) fn deny_pending(&self, id: u32) -> bool {
        let removed = {
            let mut pending = self.pending.lock().unwrap();
            let before = pending.items.len();
            pending.items.retain(|p| p.id != id);
            pending.items.len() != before
        };
        if removed {
            // Wake a parked waiter so it returns Denied now, not at timeout.
            self.changed.notify_waiters();
        }
        removed
    }

    /// Park until an operator decides on `fp_hex`, up to `timeout`.
    ///
    /// `knock_seq` is the generation [`Self::note_pending`] returned for this
    /// connection. `is_paired` is injected by the facade. Holds no lock across
    /// the await.
    pub(super) async fn wait_for_decision(
        &self,
        fp_hex: &str,
        knock_seq: u32,
        timeout: Duration,
        is_paired: impl Fn(&str) -> bool,
    ) -> PairingDecision {
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
            // Arm and enable before re-reading state so an approve/deny between
            // the check and the await cannot be lost.
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            // Once a newer knock owns the fingerprint this connection must never
            // be admitted, even if approval lands before we wake.
            match self.knock_seq_of(fp_hex) {
                Some(cur) if cur != knock_seq => return PairingDecision::Superseded,
                _ => {}
            }
            if is_paired(fp_hex) {
                // Tie-break on the admitted marker: a superseded waiter that first
                // polls after pairing sees the same paired/no-entry state as the winner.
                match self.admitted_seq(fp_hex) {
                    Some(adm) if adm != knock_seq => return PairingDecision::Superseded,
                    _ => return PairingDecision::Approved,
                }
            }
            if !self.pending_contains(fp_hex) {
                // Cleared-pending can be a denial or the facade's pin-then-clear
                // gap. Re-check is_paired; the facade pins before it clears.
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
