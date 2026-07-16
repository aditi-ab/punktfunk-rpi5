//! Shared native (`punktfunk/1`) pairing state — the on-demand arming PIN (with expiry) plus the
//! persistent paired-clients store and the delegated-approval queue. One [`NativePairing`] handle is
//! shared by the punktfunk/1 QUIC accept loop ([`crate::native`]) and the management API
//! ([`crate::mgmt`]), so an operator can **arm pairing and read the PIN from the web console**
//! instead of the service log.
//!
//! The PIN direction is inherent to the SPAKE2 ceremony: the *host* mints the PIN and the *client*
//! enters it (the client needs it to build its first message). So the UI **displays** the PIN —
//! armed on demand for a short window — rather than accepting one.
//!
//! This is a thin facade (plan §W5); the three concerns each own their state in a submodule:
//! - `arming` — the on-demand PIN window (`ArmState`),
//! - `store` — the persistent trust store (`TrustStore`),
//! - `approval` — the pending-knock queue + delegated approval (`ApprovalQueue`),
//! - `sanitize` — the untrusted-device-name scrubber.
//!
//! Admitting a device is the one cross-cutting flow: pinning the fingerprint lives in `store` and
//! clearing the pending knock lives in `approval`, so [`NativePairing::add`] drives both in order
//! (pin, THEN clear + notify) and [`NativePairing::wait_for_decision`] injects an `is_paired` closure
//! into the store-blind approval queue.

use anyhow::Result;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

mod approval;
mod arming;
mod sanitize;
mod store;

pub use approval::{PairingDecision, PendingRequest};
pub use arming::PinAttempt;
pub use store::PairedClient;

/// The untrusted-device-name sanitizer lives in its own module (plan §W5); re-exported so
/// `crate::native_pairing::sanitize_device_name` stays stable (the `native` accept loop
/// reaches it there).
pub(crate) use sanitize::sanitize_device_name;

/// Shared native-pairing state: the arming PIN window + the persistent trust store + the
/// pending-approval queue.
pub struct NativePairing {
    arm: arming::ArmState,
    store: store::TrustStore,
    approval: approval::ApprovalQueue,
}

/// A snapshot for the management API / web console.
pub struct NativePairingStatus {
    pub armed: bool,
    /// The PIN to display while armed (the operator reads it; the user enters it on the client).
    pub pin: Option<String>,
    /// Seconds left in a timed window (`None` = armed with no expiry, e.g. the CLI flag).
    pub expires_in_secs: Option<u64>,
    pub paired_clients: u32,
}

impl NativePairing {
    /// Load the trust store. `store_path = None` uses the default config path. If `arm_at_start`
    /// (the CLI `--allow-pairing`/`--require-pairing` flags), arm immediately with `fixed_pin`
    /// (or a fresh random PIN) and **no expiry** — back-compat with the headless CLI flow.
    pub fn load_with(
        store_path: Option<PathBuf>,
        fixed_pin: Option<String>,
        arm_at_start: bool,
    ) -> Result<NativePairing> {
        Ok(NativePairing {
            arm: arming::ArmState::new(arm_at_start, fixed_pin),
            store: store::TrustStore::open(store_path)?,
            approval: approval::ApprovalQueue::new(),
        })
    }

    // -- Arming window ------------------------------------------------------

    /// Arm pairing with a fresh random PIN, valid for `ttl`, **unbound** (any well-formed attempt
    /// consumes it). Returns the PIN to display. Prefer [`Self::arm_for`] with a specific device
    /// fingerprint on untrusted LANs — an unbound window is burnable by any peer (#9).
    pub fn arm(&self, ttl: Duration) -> String {
        self.arm.arm_for(ttl, None)
    }

    /// Arm pairing with a fresh random PIN, valid for `ttl`. If `bound_fp` is `Some`, the window is
    /// bound to that device fingerprint: only a pairing attempt from it consumes the window, so an
    /// unrelated (attacker) fingerprint can neither pair nor burn the window (#9). Returns the PIN.
    pub fn arm_for(&self, ttl: Duration, bound_fp: Option<String>) -> String {
        self.arm.arm_for(ttl, bound_fp)
    }

    /// Resolve the PIN for an attempt from `client_fp_hex`, honoring fingerprint binding (#9):
    /// `Disarmed` if no window is armed; `BoundToOther` if a window is armed but bound to a different
    /// fingerprint (the caller MUST reject without consuming it); else `Pin` to run the ceremony.
    pub fn pin_for_attempt(&self, client_fp_hex: &str) -> PinAttempt {
        self.arm.pin_for_attempt(client_fp_hex)
    }

    /// Disarm pairing (no new ceremonies accepted).
    pub fn disarm(&self) {
        self.arm.disarm()
    }

    /// The current valid PIN, or `None` if disarmed/expired. The QUIC ceremony reads this
    /// per-attempt, so a window that lapsed mid-connection no longer pairs.
    pub fn current_pin(&self) -> Option<String> {
        self.arm.current_pin()
    }

    /// A snapshot for the management API.
    pub fn status(&self) -> NativePairingStatus {
        let (armed, pin, expires_in_secs) = self.arm.snapshot();
        NativePairingStatus {
            armed,
            pin,
            expires_in_secs,
            paired_clients: self.store.count(),
        }
    }

    // -- Trust store --------------------------------------------------------

    /// Is this client (hex SHA-256 fingerprint) in the paired set?
    pub fn is_paired(&self, fp_hex: &str) -> bool {
        self.store.is_paired(fp_hex)
    }

    /// Record a successful pairing (re-pairing the same fingerprint just updates the name). The name
    /// is sanitized (untrusted); a persist failure rolls the in-memory store back. Pins the
    /// fingerprint in the store FIRST, then clears any pending knock for it and wakes parked waiters
    /// — an order [`Self::wait_for_decision`] relies on (a woken waiter must observe the fully
    /// settled state: paired = true, no longer pending).
    pub fn add(&self, name: &str, fp_hex: &str) -> Result<()> {
        self.store.add(name, fp_hex)?;
        self.approval.admit_and_clear(fp_hex);
        // The one choke point every successful pairing passes through (PIN ceremony AND
        // delegated approval), so the lifecycle event fires exactly once per pairing.
        crate::events::emit(crate::events::EventKind::PairingCompleted {
            device: crate::events::DeviceRef {
                name: sanitize_device_name(name, fp_hex),
                fingerprint: fp_hex.to_string(),
                plane: crate::events::Plane::Native,
            },
        });
        Ok(())
    }

    /// The paired clients (for the management API's device list).
    pub fn list(&self) -> Vec<PairedClient> {
        self.store.list()
    }

    /// Remove a paired client by fingerprint. Returns whether one was removed. On a persist
    /// failure the in-memory store is rolled back (it never diverges from disk).
    pub fn remove(&self, fp_hex: &str) -> Result<bool> {
        self.store.remove(fp_hex)
    }

    // -- Delegated approval (roadmap §8b-1) ---------------------------------

    /// Record an unpaired device's knock for delegated approval. Re-knocks from the same fingerprint
    /// refresh the existing entry in place (same id) and bump its knock generation — the returned
    /// generation is what [`Self::wait_for_decision`] admits. See [`approval::ApprovalQueue::note_pending`].
    pub fn note_pending(&self, name: &str, fp_hex: &str, src_ip: Option<IpAddr>) -> u32 {
        // Only a NEW fingerprint emits `pairing.pending` — a re-knock refreshes the existing
        // entry in place, and a client auto-retrying while parked must not spam the operator's
        // notification hook once per retry.
        let was_pending = self.approval.pending_contains(fp_hex);
        let seq = self.approval.note_pending(name, fp_hex, src_ip);
        if !was_pending {
            crate::events::emit(crate::events::EventKind::PairingPending {
                device: crate::events::DeviceRef {
                    name: sanitize_device_name(name, fp_hex),
                    fingerprint: fp_hex.to_string(),
                    plane: crate::events::Plane::Native,
                },
            });
        }
        seq
    }

    /// The devices currently awaiting approval (for the management API).
    pub fn pending(&self) -> Vec<PendingRequest> {
        self.approval.pending()
    }

    /// Is a knock for this fingerprint still awaiting approval? (Expired entries are dropped first.)
    pub fn pending_contains(&self, fp_hex: &str) -> bool {
        self.approval.pending_contains(fp_hex)
    }

    /// Approve a pending knock: pair its fingerprint (under `name_override` if the operator labeled
    /// it, else the knock's own name) and drop it from the queue. `Ok(None)` = no such (or expired)
    /// id. Reads (does NOT pre-remove) the entry, then [`Self::add`] pins the fingerprint and clears
    /// the pending entry — an order a parked waiter relies on (see [`Self::wait_for_decision`]).
    pub fn approve_pending(
        &self,
        id: u32,
        name_override: Option<&str>,
    ) -> Result<Option<PairedClient>> {
        let (knock_name, fp_hex) = match self.approval.read_entry(id) {
            Some(x) => x,
            None => return Ok(None),
        };
        let name = name_override.unwrap_or(&knock_name).to_string();
        self.add(&name, &fp_hex)?; // pins, clears the pending entry, and notifies waiters
        Ok(Some(PairedClient {
            name,
            fingerprint: fp_hex,
        }))
    }

    /// Deny (drop) a pending knock. Returns whether one was removed. The device's next knock
    /// re-creates an entry — deny is "not now", not a blocklist.
    pub fn deny_pending(&self, id: u32) -> bool {
        // Read the entry first so the lifecycle event can carry the device's identity.
        let entry = self.approval.read_entry(id);
        let denied = self.approval.deny_pending(id);
        if denied {
            if let Some((name, fp_hex)) = entry {
                crate::events::emit(crate::events::EventKind::PairingDenied {
                    device: crate::events::DeviceRef {
                        name: sanitize_device_name(&name, &fp_hex),
                        fingerprint: fp_hex,
                        plane: crate::events::Plane::Native,
                    },
                });
            }
        }
        denied
    }

    /// Park (async) until an operator decides on a knock identified by `fp_hex`, up to `timeout`.
    /// `knock_seq` is the generation [`Self::note_pending`] returned for THIS connection's knock.
    /// The store-blind approval queue is handed an `is_paired` closure so it can resolve
    /// [`PairingDecision::Approved`] the instant the fingerprint pairs. See
    /// [`approval::ApprovalQueue::wait_for_decision`] for the full decision contract.
    pub async fn wait_for_decision(
        &self,
        fp_hex: &str,
        knock_seq: u32,
        timeout: Duration,
    ) -> PairingDecision {
        self.approval
            .wait_for_decision(fp_hex, knock_seq, timeout, |fp| self.store.is_paired(fp))
            .await
    }

    /// Test-only reach into the approval queue's park flag (the behavior tests assert a parked,
    /// held-open knock survives a flood).
    #[cfg(test)]
    fn set_parked(&self, fp_hex: &str, knock_seq: u32, parked: bool) {
        self.approval.set_parked(fp_hex, knock_seq, parked)
    }
}

#[cfg(test)]
mod tests {
    use super::approval::{MAX_PENDING_PER_IP, PENDING_CAP};
    use super::*;

    fn temp() -> PathBuf {
        // A unique-ish temp path without Date/rand-in-test fuss: pid + addr of a local.
        let x = 0u8;
        std::env::temp_dir().join(format!(
            "pf-native-pair-{}-{}.json",
            std::process::id(),
            &x as *const _ as usize
        ))
    }

    #[test]
    fn arm_expire_and_pair() {
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        // Disarmed by default.
        assert!(np.current_pin().is_none());
        assert!(!np.status().armed);

        // Arm with a tiny TTL → a PIN appears, then expires.
        let pin = np.arm(Duration::from_millis(40));
        assert_eq!(pin.len(), 4);
        assert_eq!(np.current_pin().as_deref(), Some(pin.as_str()));
        assert!(np.status().armed);
        std::thread::sleep(Duration::from_millis(60));
        assert!(np.current_pin().is_none(), "window should have expired");
        assert!(!np.status().armed);

        // Pair / list / unpair.
        assert!(!np.is_paired("ab12"));
        np.add("Living Room", "AB12").unwrap();
        assert!(
            np.is_paired("ab12"),
            "fingerprint match is case-insensitive"
        );
        assert_eq!(np.list().len(), 1);
        assert_eq!(np.status().paired_clients, 1);
        assert!(np.remove("ab12").unwrap());
        assert!(!np.remove("ab12").unwrap());
        assert!(np.list().is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn pending_knock_approve_and_deny() {
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        assert!(np.pending().is_empty());

        // A knock appears; a re-knock from the same fingerprint refreshes (same id, new name)
        // instead of duplicating.
        np.note_pending("device aa11", "AA11", None);
        np.note_pending("Bedroom TV", "aa11", None);
        let pend = np.pending();
        assert_eq!(pend.len(), 1, "re-knock dedups by fingerprint");
        assert_eq!(pend[0].name, "Bedroom TV");
        let id = pend[0].id;

        // Deny drops it without pairing; the next knock gets a fresh id.
        assert!(np.deny_pending(id));
        assert!(!np.deny_pending(id));
        assert!(np.pending().is_empty());
        assert!(!np.is_paired("aa11"));

        // Approve pairs the fingerprint (operator label wins) and clears the entry.
        np.note_pending("device bb22", "BB22", None);
        let id = np.pending()[0].id;
        assert!(
            np.approve_pending(9999, None).unwrap().is_none(),
            "unknown id"
        );
        let client = np
            .approve_pending(id, Some("Living Room"))
            .unwrap()
            .unwrap();
        assert_eq!(client.name, "Living Room");
        assert!(np.is_paired("bb22"), "approval pins the fingerprint");
        assert!(np.pending().is_empty());
        assert_eq!(np.list()[0].name, "Living Room");

        // The cap evicts the oldest knock.
        // Flood from many DISTINCT source IPs (so the per-IP cap doesn't kick in) → the global cap
        // holds at PENDING_CAP, evicting the oldest non-parked entries first.
        for i in 0..(PENDING_CAP + 3) {
            let ip = IpAddr::from([10, 0, (i / 256) as u8, (i % 256) as u8]);
            np.note_pending("flood", &format!("f{i:03}"), Some(ip));
        }
        let pend = np.pending();
        assert_eq!(pend.len(), PENDING_CAP);
        assert_eq!(pend[0].fingerprint, "f003", "oldest entries evicted first");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn pairing_clears_a_pending_knock() {
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        np.note_pending("Knocker", "cc44", None);
        assert_eq!(np.pending().len(), 1);
        // Pairing the same fingerprint (e.g. via the PIN ceremony) drops the stale pending entry.
        np.add("Knocker", "CC44").unwrap();
        assert!(
            np.pending().is_empty(),
            "a now-paired device must leave the approval list"
        );
        assert!(np.is_paired("cc44"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn add_replaces_case_insensitively() {
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        np.add("First", "AB12").unwrap();
        np.add("Second", "ab12").unwrap(); // same device, different hex case
        assert_eq!(np.list().len(), 1, "re-add must replace, not duplicate");
        assert_eq!(np.list()[0].name, "Second");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn cli_flag_arms_with_no_expiry() {
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), Some("1234".into()), true).unwrap();
        assert_eq!(np.current_pin().as_deref(), Some("1234"));
        let s = np.status();
        assert!(s.armed);
        assert_eq!(s.expires_in_secs, None, "CLI arming has no expiry");
        np.disarm();
        assert!(np.current_pin().is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn wait_for_decision_approve_deny_timeout() {
        use std::sync::Arc;
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = Arc::new(NativePairing::load_with(Some(p.clone()), None, false).unwrap());

        // TimedOut: a parked knock with no decision returns TimedOut; the entry survives.
        let seq = np.note_pending("Knocker", "ab01", None);
        let d = np
            .wait_for_decision("ab01", seq, Duration::from_millis(80))
            .await;
        assert_eq!(d, PairingDecision::TimedOut);
        assert!(np.pending_contains("ab01"));

        // Approved: approving WHILE parked wakes the waiter with Approved.
        let np2 = np.clone();
        let waiter = tokio::spawn(async move {
            np2.wait_for_decision("ab01", seq, Duration::from_secs(5))
                .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let id = np
            .pending()
            .into_iter()
            .find(|x| x.fingerprint == "ab01")
            .unwrap()
            .id;
        np.approve_pending(id, Some("Approved")).unwrap().unwrap();
        assert_eq!(waiter.await.unwrap(), PairingDecision::Approved);
        assert!(np.is_paired("ab01"));

        // Denied: denying WHILE parked wakes the waiter with Denied (not held until timeout).
        let seq = np.note_pending("Knock2", "cd02", None);
        let np3 = np.clone();
        let waiter = tokio::spawn(async move {
            np3.wait_for_decision("cd02", seq, Duration::from_secs(5))
                .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let id = np
            .pending()
            .into_iter()
            .find(|x| x.fingerprint == "cd02")
            .unwrap()
            .id;
        assert!(np.deny_pending(id));
        assert_eq!(waiter.await.unwrap(), PairingDecision::Denied);
        assert!(!np.is_paired("cd02"));

        // Already paired before the call (the PIN-ceremony race) → immediate Approved: the ab01
        // marker admitted generation 0, which is also what a fresh coincidental waiter holds.
        let d = np
            .wait_for_decision("ab01", 0, Duration::from_secs(5))
            .await;
        assert_eq!(d, PairingDecision::Approved);
        let _ = std::fs::remove_file(&p);
    }

    /// One Approve must admit exactly ONE session: a re-knock supersedes the previous parked
    /// waiter (it resolves `Superseded` immediately, not at timeout), the console list keeps a
    /// single entry, and a stale-generation waiter that polls only AFTER the approval still
    /// resolves `Superseded` off the admitted marker. (Live failure this pins down: a client
    /// knocked 3×, one Approve admitted all three, and the three concurrent Mutter virtual
    /// monitors segfaulted gnome-shell.)
    #[tokio::test]
    async fn newest_knock_supersedes_parked_waiter() {
        use std::sync::Arc;
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = Arc::new(NativePairing::load_with(Some(p.clone()), None, false).unwrap());

        let seq1 = np.note_pending("iPad Pro", "ee01", None);
        let np1 = np.clone();
        let waiter1 = tokio::spawn(async move {
            np1.wait_for_decision("ee01", seq1, Duration::from_secs(5))
                .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;

        // The device retries: same fingerprint, new connection. The old waiter is superseded at
        // once; the pending list still shows ONE entry.
        let seq2 = np.note_pending("iPad Pro", "ee01", None);
        assert_ne!(seq1, seq2);
        assert_eq!(waiter1.await.unwrap(), PairingDecision::Superseded);
        assert_eq!(np.pending().len(), 1);

        let np2 = np.clone();
        let waiter2 = tokio::spawn(async move {
            np2.wait_for_decision("ee01", seq2, Duration::from_secs(5))
                .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let id = np
            .pending()
            .into_iter()
            .find(|x| x.fingerprint == "ee01")
            .unwrap()
            .id;
        np.approve_pending(id, None).unwrap().unwrap();
        assert_eq!(waiter2.await.unwrap(), PairingDecision::Approved);

        // A stale-generation waiter polling only after the approval (entry cleared, fingerprint
        // paired) must NOT read as a second Approved — the admitted marker resolves the tie.
        let d = np
            .wait_for_decision("ee01", seq1, Duration::from_millis(80))
            .await;
        assert_eq!(d, PairingDecision::Superseded);
        let _ = std::fs::remove_file(&p);
    }

    /// #9: a window can be bound to one operator-selected fingerprint, so an unrelated (attacker)
    /// fingerprint can neither pair nor BURN the window (it's rejected without a PIN).
    #[test]
    fn armed_pin_is_fingerprint_bindable() {
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        // Unbound: any fingerprint resolves to the PIN (legacy behavior).
        let pin = np.arm(Duration::from_secs(60));
        assert!(matches!(np.pin_for_attempt("aa11"), PinAttempt::Pin(x) if x == pin));
        assert!(matches!(np.pin_for_attempt("bb22"), PinAttempt::Pin(_)));
        // Bound to AA11: only that fp (case-insensitive) gets the PIN; another fp is BoundToOther —
        // the caller rejects it WITHOUT consuming the window.
        let pin = np.arm_for(Duration::from_secs(60), Some("AA11".into()));
        assert!(matches!(np.pin_for_attempt("aa11"), PinAttempt::Pin(x) if x == pin));
        assert!(matches!(
            np.pin_for_attempt("bb22"),
            PinAttempt::BoundToOther
        ));
        np.disarm();
        assert!(matches!(np.pin_for_attempt("aa11"), PinAttempt::Disarmed));
        let _ = std::fs::remove_file(&p);
    }

    /// #13: one source IP can't exceed the per-IP cap, and a parked (held-open) genuine knock is
    /// never evicted by a flood — even one that fills the global cap from many distinct IPs.
    #[test]
    fn pending_per_ip_cap_and_parked_protection() {
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        // Per-IP cap: one source flooding distinct fingerprints holds at most MAX_PENDING_PER_IP.
        let attacker = IpAddr::from([192, 168, 1, 66]);
        for i in 0..20 {
            np.note_pending("flood", &format!("atk{i:03}"), Some(attacker));
        }
        assert_eq!(
            np.pending().len(),
            MAX_PENDING_PER_IP,
            "one IP can't exceed the per-IP cap"
        );
        // A genuine knock from a different IP, parked (a live held-open connection), survives a flood
        // from many distinct IPs that fills the global cap.
        let legit = IpAddr::from([192, 168, 1, 50]);
        let seq = np.note_pending("Living Room", "legit01", Some(legit));
        np.set_parked("legit01", seq, true);
        for i in 0..(PENDING_CAP * 2) {
            let ip = IpAddr::from([10, 0, (i / 256) as u8, (i % 256) as u8]);
            np.note_pending("flood2", &format!("g{i:04}"), Some(ip));
        }
        assert!(
            np.pending_contains("legit01"),
            "a parked, held-open knock is never evicted by a flood"
        );
        assert!(np.pending().len() <= PENDING_CAP, "global cap still holds");
        let _ = std::fs::remove_file(&p);
    }
}
