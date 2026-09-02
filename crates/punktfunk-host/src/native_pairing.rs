//! Shared native (`punktfunk/1`) pairing state: the on-demand arming PIN, the persistent
//! paired-clients store, and the delegated-approval queue. One [`NativePairing`] handle is
//! shared by the QUIC accept loop ([`crate::native`]) and the management API ([`crate::mgmt`]),
//! so an operator can arm pairing and read the PIN from the web console.
//!
//! The host mints the PIN (SPAKE2); the client enters it. The UI displays a short-lived PIN
//! rather than accepting one.
//!
//! [`NativePairing::add`] pins the fingerprint first, then clears the knock;
//! [`NativePairing::wait_for_decision`] injects an `is_paired` closure into the store-blind
//! approval queue.
//!
//! [`NativePairing::is_paired`] is listing, expiry-blind. [`NativePairing::effective`] is
//! authorization right now (`None` if unpaired or expired). Admission and enforcement use
//! only `effective`. Access mutations publish [`AccessState`] on a per-fingerprint watch;
//! sessions [`NativePairing::subscribe`] at admission. Evidence: the tests below.

use anyhow::Result;
use punktfunk_core::quic::GRANT_ALL;
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::watch;

mod approval;
mod arming;
mod sanitize;
mod store;

pub use approval::{PairingDecision, PendingRequest};
pub use arming::PinAttempt;
pub use store::{Access, PairedClient};

/// What a live session observes about its device's access. Carries the record's raw deadline
/// rather than a pre-evaluated verdict — expiry is checked against the wall clock each time,
/// so "expire now" is a deadline in the past and the session's own deadline task fires on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessState {
    /// Masked grant bits. `0` when revoked.
    pub grants: u32,
    /// Unix seconds, host wall clock. `None` = permanent.
    pub deadline_unix: Option<i64>,
    /// Fingerprint is gone from the store. End sessions; do not mute them. Distinct from
    /// view-only (`grants == 0`, `revoked == false`), where the session stays as a spectator.
    pub revoked: bool,
}

/// Re-exported for the stream marker's quoting. `imp` is `cfg(unix)` — gate alike, or
/// Windows trips `-D unused-imports`.
#[cfg(unix)]
pub(crate) use sanitize::is_spoofy_char;
/// Stable path for the native accept loop.
pub(crate) use sanitize::sanitize_device_name;

/// Host wall clock, unix seconds. An NTP step moves stored deadlines with the clock.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct NativePairing {
    arm: arming::ArmState,
    store: store::TrustStore,
    approval: approval::ApprovalQueue,
    /// Fingerprint (lowercased) → live-session channel. Senders stay after unpair so a
    /// late subscriber cannot race a close; a re-pair publishes on the same channel.
    access_watch: Mutex<HashMap<String, watch::Sender<AccessState>>>,
}

pub struct NativePairingStatus {
    pub armed: bool,
    pub pin: Option<String>,
    /// Seconds left. `None` = no expiry (CLI `--allow-pairing`).
    pub expires_in_secs: Option<u64>,
    pub paired_clients: u32,
}

impl NativePairing {
    /// Load the trust store. `store_path = None` uses the default path. `arm_at_start` (CLI
    /// `--allow-pairing` / `--require-pairing`) arms immediately with `fixed_pin` or a random
    /// PIN and no expiry.
    pub fn load_with(
        store_path: Option<PathBuf>,
        fixed_pin: Option<String>,
        arm_at_start: bool,
    ) -> Result<NativePairing> {
        Ok(NativePairing {
            arm: arming::ArmState::new(arm_at_start, fixed_pin),
            store: store::TrustStore::open(store_path)?,
            approval: approval::ApprovalQueue::new(),
            access_watch: Mutex::new(HashMap::new()),
        })
    }

    /// Arm with a fresh PIN for `ttl`, unbound (any well-formed attempt consumes it) and with
    /// no access choice. Prefer [`Self::arm_for`] on untrusted LANs — an unbound window is
    /// burnable by any peer.
    pub fn arm(&self, ttl: Duration) -> String {
        self.arm.arm_for(ttl, None, None)
    }

    /// Arm with a fresh PIN for `ttl`. `bound_fp` restricts consumption to that fingerprint;
    /// another peer can neither pair nor burn the window. `access` is the operator's grant
    /// for whoever completes the ceremony; `None` = full/permanent default.
    pub fn arm_for(
        &self,
        ttl: Duration,
        bound_fp: Option<String>,
        access: Option<Access>,
    ) -> String {
        self.arm.arm_for(ttl, bound_fp, access)
    }

    /// Access choice on the current window (`None` if disarmed, expired, or unset). The PIN
    /// ceremony reads this before consuming the window; [`Self::disarm`] wipes it with the PIN.
    pub fn armed_access(&self) -> Option<Access> {
        self.arm.armed_access()
    }

    /// PIN for an attempt from `client_fp_hex`. `BoundToOther` means reject without consuming
    /// the window; `Disarmed` means none is armed.
    pub fn pin_for_attempt(&self, client_fp_hex: &str) -> PinAttempt {
        self.arm.pin_for_attempt(client_fp_hex)
    }

    pub fn disarm(&self) {
        self.arm.disarm()
    }

    /// Current valid PIN, or `None` if disarmed/expired. Read per attempt so a window that
    /// lapses mid-connection no longer pairs.
    pub fn current_pin(&self) -> Option<String> {
        self.arm.current_pin()
    }

    pub fn status(&self) -> NativePairingStatus {
        let (armed, pin, expires_in_secs) = self.arm.snapshot();
        NativePairingStatus {
            armed,
            pin,
            expires_in_secs,
            paired_clients: self.store.count(),
        }
    }

    /// Listed in the paired set, expiry-blind. An expired guest still shows in the device list
    /// and still short-circuits the approval queue. Admission and enforcement use
    /// [`Self::effective`].
    pub fn is_paired(&self, fp_hex: &str) -> bool {
        self.store.is_paired(fp_hex)
    }

    /// Grant mask authorized at `now_unix` (host wall clock, unix seconds): `None` if unpaired
    /// or expired. Absent grants mean full control; reserved bits are already masked.
    pub fn effective(&self, fp_hex: &str, now_unix: i64) -> Option<u32> {
        self.store.effective(fp_hex, now_unix)
    }

    /// GameStream grant for this fingerprint. No row means ungoverned full control — that plane's
    /// pairing authority is its cert list, not this store. A row that exists governs as native
    /// (`effective`): mask, reserved bits cleared, expiry → `None`. One snapshot: `is_paired`
    /// then `effective` can race a delete into "listed but expired" for a just-ungoverned row.
    pub fn moonlight_effective(&self, fp_hex: &str, now_unix: i64) -> Option<u32> {
        match self.store.get(fp_hex) {
            None => Some(GRANT_ALL),
            Some(c) => {
                if c.expires_unix.is_some_and(|t| now_unix >= t) {
                    None
                } else {
                    Some(
                        punktfunk_core::quic::normalize_legacy_full(c.grants.unwrap_or(GRANT_ALL))
                            & GRANT_ALL,
                    )
                }
            }
        }
    }

    /// Record a successful pairing with no explicit access choice. New fingerprint: full
    /// permanent default. Existing: name-only — grants and expiry stay, so a limited guest
    /// cannot re-pair itself to full control. Widening goes through [`Self::add_with_access`],
    /// [`Self::set_access`], or [`Self::approve_pending`].
    pub fn add(&self, name: &str, fp_hex: &str) -> Result<()> {
        self.add_with_access(name, fp_hex, None)
    }

    /// Record a successful pairing. `Some(access)` replaces the record; `None` preserves as
    /// [`Self::add`]. Persist failure rolls the in-memory store back. Pins first, then clears
    /// the knock and wakes waiters — [`Self::wait_for_decision`] requires a woken waiter to
    /// observe paired and no longer pending — then publishes to live watchers.
    pub fn add_with_access(&self, name: &str, fp_hex: &str, access: Option<Access>) -> Result<()> {
        self.store.add_with_access(name, fp_hex, access)?;
        self.approval.admit_and_clear(fp_hex);
        // Every successful pairing (PIN and delegated approval) passes here, so
        // `pairing.completed` fires once.
        let device = crate::events::DeviceRef {
            name: sanitize_device_name(name, fp_hex),
            fingerprint: fp_hex.to_string(),
            plane: crate::events::Plane::Native,
        };
        crate::events::emit(crate::events::EventKind::PairingCompleted {
            device: device.clone(),
        });
        // `access.granted` only for an explicit operator choice (approve dialog, arm
        // window). Read the stored record: reserved bits are already masked and grant
        // time stamped. A choice-less pairing emits only `pairing.completed` above.
        if access.is_some() {
            if let Some(stored) = self.store.get(fp_hex) {
                crate::events::emit(crate::events::EventKind::AccessGranted {
                    device,
                    grants: stored.grants.unwrap_or(GRANT_ALL) & GRANT_ALL,
                    expires_unix: stored.expires_unix,
                });
            }
        }
        self.publish_current(fp_hex);
        Ok(())
    }

    /// Overwrite a paired device's access, persist, then publish to live sessions in one
    /// watch event. Returns `false` (no write, no publish) for an unknown fingerprint —
    /// editing access is not a way to pair.
    pub fn set_access(&self, fp_hex: &str, access: Access) -> Result<bool> {
        if !self.store.set_access(fp_hex, access)? {
            return Ok(false);
        }
        // Read the stored record so hooks see masked bits actually in force.
        if let Some(stored) = self.store.get(fp_hex) {
            crate::events::emit(crate::events::EventKind::AccessChanged {
                device: crate::events::DeviceRef {
                    name: stored.name,
                    fingerprint: fp_hex.to_ascii_lowercase(),
                    plane: crate::events::Plane::Native,
                },
                grants: stored.grants.unwrap_or(GRANT_ALL) & GRANT_ALL,
                expires_unix: stored.expires_unix,
            });
        }
        self.publish_current(fp_hex);
        Ok(true)
    }

    /// Subscribe to this fingerprint's access. Current value is the state now (unpaired ⇒
    /// already `revoked`); later pair/edit/unpair arrive as change notifications.
    pub fn subscribe(&self, fp_hex: &str) -> watch::Receiver<AccessState> {
        let mut map = self.access_watch.lock().unwrap();
        map.entry(fp_hex.to_ascii_lowercase())
            .or_insert_with(|| watch::channel(self.current_state(fp_hex)).0)
            .subscribe()
    }

    fn current_state(&self, fp_hex: &str) -> AccessState {
        match self.store.get(fp_hex) {
            Some(c) => AccessState {
                grants: c.grants.unwrap_or(GRANT_ALL) & GRANT_ALL,
                deadline_unix: c.expires_unix,
                revoked: false,
            },
            None => AccessState {
                grants: 0,
                deadline_unix: None,
                revoked: true,
            },
        }
    }

    /// Publish `fp_hex` to its watchers. No-op if nobody subscribed and nothing mutated it:
    /// the channel is minted lazily. Same-value mutations wake nobody.
    fn publish_current(&self, fp_hex: &str) {
        let state = self.current_state(fp_hex);
        let mut map = self.access_watch.lock().unwrap();
        let tx = map
            .entry(fp_hex.to_ascii_lowercase())
            .or_insert_with(|| watch::channel(state).0);
        tx.send_if_modified(|cur| {
            if *cur == state {
                false
            } else {
                *cur = state;
                true
            }
        });
    }

    pub fn list(&self) -> Vec<PairedClient> {
        self.store.list()
    }

    /// Remove by fingerprint. Persist failure rolls the in-memory store back. A removal
    /// publishes `revoked` so live sessions can end themselves.
    pub fn remove(&self, fp_hex: &str) -> Result<bool> {
        let removed = self.store.remove(fp_hex)?;
        if removed {
            self.publish_current(fp_hex);
        }
        Ok(removed)
    }

    /// Remove every paired client in one write. Returns the fingerprints so the caller can
    /// end those sessions. Persist failure removes nothing. Publishes `revoked` per row.
    pub fn remove_all(&self) -> Result<Vec<String>> {
        let removed = self.store.remove_all()?;
        for fp in &removed {
            self.publish_current(fp);
        }
        Ok(removed)
    }

    /// Record an unpaired knock. A re-knock from the same fingerprint refreshes in place
    /// (same id, new generation). The generation is what [`Self::wait_for_decision`] admits.
    pub fn note_pending(&self, name: &str, fp_hex: &str, src_ip: Option<IpAddr>) -> u32 {
        // Only a new fingerprint emits `pairing.pending`. A parked client's retries
        // must not notify the operator once per attempt.
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

    pub fn pending(&self) -> Vec<PendingRequest> {
        self.approval.pending()
    }

    /// Drops expired entries first.
    pub fn pending_contains(&self, fp_hex: &str) -> bool {
        self.approval.pending_contains(fp_hex)
    }

    /// Approve a pending knock: pair under `name_override` (else the knock's name) and drop
    /// the queue entry. `access` is the dialog choice; `None` keeps existing access or the
    /// full/permanent default. `Ok(None)` = unknown or expired id. Reads the entry (does not
    /// pre-remove), then [`Self::add_with_access`] pins and clears — waiters rely on that order.
    pub fn approve_pending(
        &self,
        id: u32,
        name_override: Option<&str>,
        access: Option<Access>,
    ) -> Result<Option<PairedClient>> {
        let (knock_name, fp_hex) = match self.approval.read_entry(id) {
            Some(x) => x,
            None => return Ok(None),
        };
        let name = name_override.unwrap_or(&knock_name).to_string();
        self.add_with_access(&name, &fp_hex, access)?;

        // Read the stored record: `access == None` on a re-pair kept prior grants/expiry.
        Ok(self.store.get(&fp_hex))
    }

    /// Deny is "not now": the next knock creates a new entry.
    pub fn deny_pending(&self, id: u32) -> bool {
        // Identity for the lifecycle event; deny after this would lose the name.
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

    /// Park until an operator decides on `fp_hex`, up to `timeout`. `knock_seq` is the
    /// generation [`Self::note_pending`] returned for this connection. The queue is store-blind;
    /// the paired-check closure resolves [`PairingDecision::Approved`] the instant the
    /// fingerprint is authorized. See [`approval::ApprovalQueue::wait_for_decision`].
    ///
    /// The closure uses [`Self::effective`], not [`Self::is_paired`]: an expired guest is still
    /// listed, and resolving on listing would admit the knock before re-grant, then fail
    /// admission with a typed expiry close. A bare PIN re-pair preserves expired access and
    /// does not admit a parked knock either.
    pub async fn wait_for_decision(
        &self,
        fp_hex: &str,
        knock_seq: u32,
        timeout: Duration,
    ) -> PairingDecision {
        self.approval
            .wait_for_decision(fp_hex, knock_seq, timeout, |fp| {
                self.store.effective(fp, unix_now()).is_some()
            })
            .await
    }

    /// Test-only park flag. Behavior tests assert a parked knock survives a flood.
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
        // Unique-enough path without pulling rand into tests: pid + address of a stack byte.
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
        assert!(np.current_pin().is_none());
        assert!(!np.status().armed);

        let pin = np.arm(Duration::from_millis(40));
        assert_eq!(pin.len(), 4);
        assert_eq!(np.current_pin().as_deref(), Some(pin.as_str()));
        assert!(np.status().armed);
        std::thread::sleep(Duration::from_millis(60));
        assert!(np.current_pin().is_none(), "window should have expired");
        assert!(!np.status().armed);

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

        np.note_pending("device aa11", "AA11", None);
        np.note_pending("Bedroom TV", "aa11", None);
        let pend = np.pending();
        assert_eq!(pend.len(), 1, "re-knock dedups by fingerprint");
        assert_eq!(pend[0].name, "Bedroom TV");
        let id = pend[0].id;

        assert!(np.deny_pending(id));
        assert!(!np.deny_pending(id));
        assert!(np.pending().is_empty());
        assert!(!np.is_paired("aa11"));

        np.note_pending("device bb22", "BB22", None);
        let id = np.pending()[0].id;
        assert!(
            np.approve_pending(9999, None, None).unwrap().is_none(),
            "unknown id"
        );
        let client = np
            .approve_pending(id, Some("Living Room"), None)
            .unwrap()
            .unwrap();
        assert_eq!(client.name, "Living Room");
        assert!(np.is_paired("bb22"), "approval pins the fingerprint");
        assert!(np.pending().is_empty());
        assert_eq!(np.list()[0].name, "Living Room");

        // Distinct source IPs so the per-IP cap does not fire; the global cap holds
        // at PENDING_CAP and evicts the oldest non-parked entries first.
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
        np.add("Second", "ab12").unwrap();
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

        let seq = np.note_pending("Knocker", "ab01", None);
        let d = np
            .wait_for_decision("ab01", seq, Duration::from_millis(80))
            .await;
        assert_eq!(d, PairingDecision::TimedOut);
        assert!(np.pending_contains("ab01"));

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
        np.approve_pending(id, Some("Approved"), None)
            .unwrap()
            .unwrap();
        assert_eq!(waiter.await.unwrap(), PairingDecision::Approved);
        assert!(np.is_paired("ab01"));

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

        // Already paired (PIN-ceremony race): generation 0 matches a coincidental waiter
        // and resolves Approved immediately.
        let d = np
            .wait_for_decision("ab01", 0, Duration::from_secs(5))
            .await;
        assert_eq!(d, PairingDecision::Approved);
        let _ = std::fs::remove_file(&p);
    }

    /// An expired record is still listed. A paired-check on listing would admit the knock
    /// before re-grant; the session would then die on admission's effective-check. Only
    /// operator re-approval (which refreshes access) may resolve the waiter.
    #[tokio::test]
    async fn expired_record_parks_until_regrant() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        use std::sync::Arc;
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = Arc::new(NativePairing::load_with(Some(p.clone()), None, false).unwrap());
        np.add_with_access(
            "Old Guest",
            "aa77",
            Some(Access {
                grants: GRANT_ALL,
                expires_unix: Some(wall_now() - 10),
            }),
        )
        .unwrap();
        assert!(np.is_paired("aa77"), "expired but still listed");

        let seq = np.note_pending("Old Guest", "aa77", None);
        let d = np
            .wait_for_decision("aa77", seq, Duration::from_millis(120))
            .await;
        assert_eq!(d, PairingDecision::TimedOut);

        let seq = np.note_pending("Old Guest", "aa77", None);
        let np2 = np.clone();
        let waiter = tokio::spawn(async move {
            np2.wait_for_decision("aa77", seq, Duration::from_secs(5))
                .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let id = np
            .pending()
            .into_iter()
            .find(|x| x.fingerprint == "aa77")
            .unwrap()
            .id;
        np.approve_pending(
            id,
            None,
            Some(Access {
                grants: GRANT_GAMEPAD,
                expires_unix: Some(wall_now() + 3600),
            }),
        )
        .unwrap()
        .unwrap();
        assert_eq!(waiter.await.unwrap(), PairingDecision::Approved);
        assert_eq!(np.effective("aa77", wall_now()), Some(GRANT_GAMEPAD));
        let _ = std::fs::remove_file(&p);
    }

    /// One Approve admits exactly one session. A re-knock supersedes the previous parked
    /// waiter immediately. A stale-generation waiter that polls only after approval still
    /// resolves `Superseded` off the admitted marker.
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
        np.approve_pending(id, None, None).unwrap().unwrap();
        assert_eq!(waiter2.await.unwrap(), PairingDecision::Approved);

        // After approval the entry is gone and the fingerprint is paired; the admitted
        // marker must still resolve this stale generation as Superseded, not Approved.
        let d = np
            .wait_for_decision("ee01", seq1, Duration::from_millis(80))
            .await;
        assert_eq!(d, PairingDecision::Superseded);
        let _ = std::fs::remove_file(&p);
    }

    /// A window bound to one fingerprint: another peer can neither pair nor burn it
    /// (rejected without a PIN).
    #[test]
    fn armed_pin_is_fingerprint_bindable() {
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        let pin = np.arm(Duration::from_secs(60));
        assert!(matches!(np.pin_for_attempt("aa11"), PinAttempt::Pin(x) if x == pin));
        assert!(matches!(np.pin_for_attempt("bb22"), PinAttempt::Pin(_)));
        let pin = np.arm_for(Duration::from_secs(60), Some("AA11".into()), None);
        assert!(matches!(np.pin_for_attempt("aa11"), PinAttempt::Pin(x) if x == pin));
        assert!(matches!(
            np.pin_for_attempt("bb22"),
            PinAttempt::BoundToOther
        ));
        np.disarm();
        assert!(matches!(np.pin_for_attempt("aa11"), PinAttempt::Disarmed));
        let _ = std::fs::remove_file(&p);
    }

    /// One source IP cannot exceed the per-IP cap. A parked genuine knock is never
    /// evicted by a flood, even one that fills the global cap from many IPs.
    #[test]
    fn pending_per_ip_cap_and_parked_protection() {
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        let attacker = IpAddr::from([192, 168, 1, 66]);
        for i in 0..20 {
            np.note_pending("flood", &format!("atk{i:03}"), Some(attacker));
        }
        assert_eq!(
            np.pending().len(),
            MAX_PENDING_PER_IP,
            "one IP can't exceed the per-IP cap"
        );
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

    fn wall_now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// A store written before grants existed (name + fingerprint only) decodes as full
    /// control, forever.
    #[test]
    fn pre_grants_store_decodes_as_full_permanent() {
        let p = temp();
        let _ = std::fs::remove_file(&p);
        std::fs::write(
            &p,
            br#"{ "clients": [ { "name": "Old Laptop", "fingerprint": "ab12" } ] }"#,
        )
        .unwrap();
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        let listed = np.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Old Laptop");
        assert_eq!(listed[0].grants, None, "absent stays absent");
        assert_eq!(listed[0].expires_unix, None);
        assert_eq!(listed[0].granted_unix, None);
        assert!(np.is_paired("AB12"));
        assert_eq!(
            np.effective("AB12", wall_now()),
            Some(GRANT_ALL),
            "absent grants = full control"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// Re-running the pairing ceremony must never widen access. `add()` is name-only for
    /// an existing fingerprint.
    #[test]
    fn repair_via_add_never_escalates() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        let now = wall_now();
        let guest = Access {
            grants: GRANT_GAMEPAD,
            expires_unix: Some(now + 3600),
        };
        np.add_with_access("Guest Deck", "aa11", Some(guest))
            .unwrap();
        let before = np.list()[0].clone();
        assert_eq!(before.grants, Some(GRANT_GAMEPAD));
        assert_eq!(before.expires_unix, Some(now + 3600));
        assert!(before.granted_unix.is_some());

        np.add("Guest Deck Again", "AA11").unwrap();
        assert_eq!(np.list().len(), 1, "re-pair must not duplicate");
        let after = np.list()[0].clone();
        assert_eq!(after.name, "Guest Deck Again");
        assert_eq!(after.grants, before.grants, "re-pair must NOT touch grants");
        assert_eq!(
            after.expires_unix, before.expires_unix,
            "re-pair must NOT touch expiry"
        );
        assert_eq!(
            after.granted_unix, before.granted_unix,
            "re-pair must NOT re-stamp the grant time"
        );
        assert_eq!(
            np.effective("aa11", now),
            Some(GRANT_GAMEPAD),
            "still controller-only after the re-pair"
        );

        // Limitation is the persisted record, not memory.
        drop(np);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        assert_eq!(np.effective("aa11", now), Some(GRANT_GAMEPAD));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn approve_with_access_pins_the_choice() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        let now = wall_now();
        np.note_pending("device bb22", "BB22", None);
        let id = np.pending()[0].id;
        let client = np
            .approve_pending(
                id,
                Some("Guest Phone"),
                Some(Access {
                    grants: GRANT_GAMEPAD,
                    expires_unix: Some(now + 4 * 3600),
                }),
            )
            .unwrap()
            .unwrap();
        assert_eq!(client.name, "Guest Phone");
        assert_eq!(client.grants, Some(GRANT_GAMEPAD));
        assert_eq!(client.expires_unix, Some(now + 4 * 3600));
        assert!(client.granted_unix.is_some());
        assert_eq!(np.effective("bb22", now), Some(GRANT_GAMEPAD));
        let _ = std::fs::remove_file(&p);
    }

    /// Expiry ends authorization, not listing: `effective()` becomes `None` at the
    /// deadline while `is_paired()`/`list()` keep the row.
    #[test]
    fn expiry_flips_effective_but_keeps_the_row() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        let now = wall_now();
        np.add_with_access(
            "Evening Guest",
            "cc33",
            Some(Access {
                grants: GRANT_GAMEPAD,
                expires_unix: Some(now + 10),
            }),
        )
        .unwrap();
        assert_eq!(np.effective("cc33", now), Some(GRANT_GAMEPAD));
        assert_eq!(
            np.effective("cc33", now + 9),
            Some(GRANT_GAMEPAD),
            "still authorized one second before the deadline"
        );
        assert_eq!(
            np.effective("cc33", now + 10),
            None,
            "the deadline itself expires"
        );
        assert_eq!(np.effective("cc33", now + 3600), None);
        assert!(np.is_paired("cc33"), "expired but still LISTED");
        assert_eq!(np.list().len(), 1, "the row survives for the console");
        let _ = std::fs::remove_file(&p);
    }

    /// A store that smuggles reserved bits cannot feed them into enforcement:
    /// `effective()` and the watch mask with GRANT_ALL on read.
    #[test]
    fn reserved_bits_are_masked_on_read() {
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        np.add("Future Device", "dd44").unwrap();
        let from_the_future = Access {
            grants: GRANT_ALL | (1 << 30),
            expires_unix: None,
        };
        assert!(np.set_access("dd44", from_the_future).unwrap());
        assert_eq!(
            np.effective("dd44", wall_now()),
            Some(GRANT_ALL),
            "reserved bit masked off on read"
        );
        assert_eq!(np.subscribe("dd44").borrow().grants, GRANT_ALL);
        assert!(!np.set_access("nope99", from_the_future).unwrap());
        assert!(!np.is_paired("nope99"));
        let _ = std::fs::remove_file(&p);
    }

    /// An edit reaches a live subscriber in one event; unpair publishes `revoked`.
    /// Re-pairing publishes fresh state on the same channel.
    #[tokio::test]
    async fn watch_publishes_on_set_access_and_unpair() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        np.add("Living Room", "ee55").unwrap();
        // Registry keys case-insensitively, like the store.
        let mut rx = np.subscribe("EE55");
        assert_eq!(
            *rx.borrow(),
            AccessState {
                grants: GRANT_ALL,
                deadline_unix: None,
                revoked: false
            },
            "initial value is the state now"
        );

        let now = wall_now();
        np.set_access(
            "ee55",
            Access {
                grants: GRANT_GAMEPAD,
                expires_unix: Some(now + 60),
            },
        )
        .unwrap();
        rx.changed().await.unwrap();
        assert_eq!(
            *rx.borrow(),
            AccessState {
                grants: GRANT_GAMEPAD,
                deadline_unix: Some(now + 60),
                revoked: false
            }
        );

        assert!(np.remove("ee55").unwrap());
        rx.changed().await.unwrap();
        assert_eq!(
            *rx.borrow(),
            AccessState {
                grants: 0,
                deadline_unix: None,
                revoked: true
            }
        );

        // Same channel: a stale-but-alive subscriber sees the new state.
        np.add("Living Room", "ee55").unwrap();
        rx.changed().await.unwrap();
        assert!(!rx.borrow().revoked);
        assert_eq!(rx.borrow().grants, GRANT_ALL);

        assert!(np.subscribe("zz99").borrow().revoked);
        let _ = std::fs::remove_file(&p);
    }

    /// Absent Moonlight record = ungoverned full control (the GameStream cert list is
    /// pairing authority). A record that exists governs as native, including expiry
    /// failing closed. Flipping the absent arm to `None` would revoke every ungoverned
    /// Moonlight pairing on upgrade.
    #[test]
    fn moonlight_effective_absent_is_ungoverned_but_a_record_governs() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        let now = wall_now();

        assert_eq!(np.effective("ab12", now), None, "native: unpaired");
        assert_eq!(
            np.moonlight_effective("ab12", now),
            Some(GRANT_ALL),
            "moonlight: ungoverned = full control"
        );

        np.add_with_access(
            "Guest Deck",
            "AB12",
            Some(Access {
                grants: GRANT_GAMEPAD,
                expires_unix: Some(now + 60),
            }),
        )
        .unwrap();
        assert_eq!(np.moonlight_effective("ab12", now), Some(GRANT_GAMEPAD));
        assert_eq!(
            np.moonlight_effective("ab12", now),
            np.effective("ab12", now)
        );

        assert_eq!(np.moonlight_effective("ab12", now + 60), None);
        assert_eq!(
            np.moonlight_effective("ab12", now + 60),
            np.effective("ab12", now + 60)
        );

        // Unpair here returns ungoverned; GameStream pairing is a separate store.
        assert!(np.remove("ab12").unwrap());
        assert_eq!(np.moonlight_effective("ab12", now), Some(GRANT_ALL));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn armed_window_carries_access_until_consumed() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        assert_eq!(np.armed_access(), None, "disarmed = no choice");
        let choice = Access {
            grants: GRANT_GAMEPAD,
            expires_unix: Some(wall_now() + 3600),
        };
        np.arm_for(Duration::from_secs(60), None, Some(choice));
        assert_eq!(np.armed_access(), Some(choice));
        np.disarm();
        assert_eq!(np.armed_access(), None, "consumed with the window");
        let _ = std::fs::remove_file(&p);
    }
}
