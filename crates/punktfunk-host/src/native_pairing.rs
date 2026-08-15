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
//!
//! **Two verbs, two questions** (per-client access, design §3–§5). [`NativePairing::is_paired`]
//! answers "is this device *listed*?" — in the store, expiry-blind; it feeds the approval queue's
//! decision closure, the status count, and the device list, where an expired guest must still
//! appear (as "Expired") rather than vanish. [`NativePairing::effective`] answers "what is this
//! device *authorized* for right now?" — `None` when unpaired or expired, else the grant mask —
//! and is the only verb admission and enforcement may consult. Don't substitute one for the other.
//!
//! Beside the store sits the **access watch registry**: one `tokio::sync::watch` channel per
//! fingerprint carrying [`AccessState`]. Every access mutation (pair, edit, unpair) publishes
//! through it, so a console edit or unpair reaches every live session from that fingerprint
//! within one event (design §5.6); sessions [`NativePairing::subscribe`] at admission.

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

/// What a live session observes about its device's access, published through the per-fingerprint
/// watch channel. Carries the record's *raw* deadline rather than a pre-evaluated verdict —
/// expiry is evaluated against the wall clock at each check (design §4), so an "expire now" edit
/// is just a deadline in the past and the session's own deadline task fires on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessState {
    /// Effective grant mask, reserved bits already cleared. `0` for a revoked device.
    pub grants: u32,
    /// Absolute deadline, unix seconds host wall clock. `None` = permanent.
    pub deadline_unix: Option<i64>,
    /// The fingerprint is no longer in the store at all (unpaired) — terminal for the device's
    /// sessions: end them, don't merely mute them. Distinct from an edit to view-only
    /// (`grants == 0, revoked == false`), where the session survives as a spectator.
    pub revoked: bool,
}

/// Re-exported for the stream marker's quoting (its `imp` is `cfg(unix)` — gate alike, or the
/// Windows build trips `-D unused-imports`).
#[cfg(unix)]
pub(crate) use sanitize::is_spoofy_char;
/// The untrusted-device-name sanitizer lives in its own module (plan §W5); re-exported so
/// `crate::native_pairing::sanitize_device_name` stays stable (the `native` accept loop
/// reaches it there).
pub(crate) use sanitize::sanitize_device_name;

/// Host wall clock, unix seconds — the clock access deadlines are stored in and evaluated
/// against (design §4: wall time at each check, so an NTP step moves a deadline with the clock).
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Shared native-pairing state: the arming PIN window + the persistent trust store + the
/// pending-approval queue.
pub struct NativePairing {
    arm: arming::ArmState,
    store: store::TrustStore,
    approval: approval::ApprovalQueue,
    /// The access watch registry: fingerprint (lowercased) → the channel its live sessions
    /// observe. Senders are retained after unpair (the map is bounded by fingerprints ever
    /// seen, and retaining avoids a publish/close race with late subscribers); a re-pair
    /// publishes fresh state on the same channel.
    access_watch: Mutex<HashMap<String, watch::Sender<AccessState>>>,
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
            access_watch: Mutex::new(HashMap::new()),
        })
    }

    // -- Arming window ------------------------------------------------------

    /// Arm pairing with a fresh random PIN, valid for `ttl`, **unbound** (any well-formed attempt
    /// consumes it) and with no access choice (whoever pairs gets the full/permanent default).
    /// Returns the PIN to display. Prefer [`Self::arm_for`] with a specific device fingerprint on
    /// untrusted LANs — an unbound window is burnable by any peer (#9).
    pub fn arm(&self, ttl: Duration) -> String {
        self.arm.arm_for(ttl, None, None)
    }

    /// Arm pairing with a fresh random PIN, valid for `ttl`. If `bound_fp` is `Some`, the window is
    /// bound to that device fingerprint: only a pairing attempt from it consumes the window, so an
    /// unrelated (attacker) fingerprint can neither pair nor burn the window (#9). `access` is the
    /// operator's choice for whichever device completes this window's ceremony (the arm dialog is
    /// one of the three authorized grant paths, design §5.7); `None` = the full/permanent default.
    /// Returns the PIN.
    pub fn arm_for(
        &self,
        ttl: Duration,
        bound_fp: Option<String>,
        access: Option<Access>,
    ) -> String {
        self.arm.arm_for(ttl, bound_fp, access)
    }

    /// The access choice the current armed window carries (`None` when disarmed, expired, or armed
    /// without a choice). The PIN ceremony reads this **before** consuming the single-use window —
    /// [`Self::disarm`] wipes it along with the PIN.
    pub fn armed_access(&self) -> Option<Access> {
        self.arm.armed_access()
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

    /// Is this client (hex SHA-256 fingerprint) in the paired set? **Listed, not authorized**:
    /// expiry-blind on purpose (an expired guest still shows in the device list and still
    /// short-circuits the approval queue's paired-check). Admission and enforcement must ask
    /// [`Self::effective`] instead — see the module header's two-verbs contract.
    pub fn is_paired(&self, fp_hex: &str) -> bool {
        self.store.is_paired(fp_hex)
    }

    /// The grant mask this fingerprint is authorized for at `now_unix` (host wall clock, unix
    /// seconds): `None` when unpaired OR expired; `Some(mask)` otherwise, with absent grants
    /// meaning full control and reserved bits already masked off. The admission gate's verb
    /// (WP3) — see the module header's two-verbs contract.
    pub fn effective(&self, fp_hex: &str, now_unix: i64) -> Option<u32> {
        self.store.effective(fp_hex, now_unix)
    }

    /// Record a successful pairing with no explicit access choice: for a NEW fingerprint, the
    /// full/permanent default; for an existing one, **name-only** — grants and expiry are
    /// preserved, so a limited guest re-running the ceremony cannot escalate itself back to full
    /// control (design §5.7). Access is chosen through [`Self::add_with_access`] /
    /// [`Self::set_access`] / [`Self::approve_pending`], all operator-driven.
    pub fn add(&self, name: &str, fp_hex: &str) -> Result<()> {
        self.add_with_access(name, fp_hex, None)
    }

    /// Record a successful pairing, optionally with the operator's access choice (`Some` replaces
    /// the record's access — the operator just chose anew; `None` is [`Self::add`]'s preserving
    /// behavior). The name is sanitized (untrusted); a persist failure rolls the in-memory store
    /// back. Pins the fingerprint in the store FIRST, then clears any pending knock for it and
    /// wakes parked waiters — an order [`Self::wait_for_decision`] relies on (a woken waiter must
    /// observe the fully settled state: paired = true, no longer pending) — then publishes the
    /// (possibly refreshed) access to any live watchers.
    pub fn add_with_access(&self, name: &str, fp_hex: &str, access: Option<Access>) -> Result<()> {
        self.store.add_with_access(name, fp_hex, access)?;
        self.approval.admit_and_clear(fp_hex);
        // The one choke point every successful pairing passes through (PIN ceremony AND
        // delegated approval), so the lifecycle event fires exactly once per pairing.
        let device = crate::events::DeviceRef {
            name: sanitize_device_name(name, fp_hex),
            fingerprint: fp_hex.to_string(),
            plane: crate::events::Plane::Native,
        };
        crate::events::emit(crate::events::EventKind::PairingCompleted {
            device: device.clone(),
        });
        // `access.granted` fires only for an EXPLICIT operator choice (approve dialog, arm
        // window) — this is likewise the one choke point both of those pass through. Read the
        // stored record back for the payload: the store masked reserved bits and stamped the
        // grant time, so the event must report what is actually in force. A choice-less pairing
        // (preserved or default access) emits only `pairing.completed` above.
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

    /// Overwrite a paired device's access (the console edit sheet / extend / "expire now"):
    /// persists to the store, then publishes to the fingerprint's live sessions — within one
    /// watch event, per design §5.6. Returns `false` (writing and publishing nothing) for an
    /// unknown fingerprint: editing access is not a way to pair a device.
    pub fn set_access(&self, fp_hex: &str, access: Access) -> Result<bool> {
        if !self.store.set_access(fp_hex, access)? {
            return Ok(false);
        }
        // The edit-sheet choke point (design §6 events): read the stored record back — the store
        // is what masked reserved bits — so hooks see what is actually in force.
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

    /// Subscribe to a fingerprint's access — sessions call this at admission (WP3) and fold every
    /// change into their live enforcement mask/deadline. The receiver's current value is the
    /// state *now* (unpaired ⇒ already `revoked`); every subsequent mutation (pair, edit, unpair)
    /// arrives as a change notification. Grants-blind callers never need this; it exists for the
    /// session lifecycle.
    pub fn subscribe(&self, fp_hex: &str) -> watch::Receiver<AccessState> {
        let mut map = self.access_watch.lock().unwrap();
        map.entry(fp_hex.to_ascii_lowercase())
            .or_insert_with(|| watch::channel(self.current_state(fp_hex)).0)
            .subscribe()
    }

    /// The watch payload for a fingerprint as the store stands: the record's masked grants and
    /// raw deadline, or the terminal `revoked` state when it isn't listed.
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

    /// Publish the store's current state for `fp_hex` to its watchers (deduplicated — an access
    /// mutation that lands on the same value wakes nobody). No-op when nothing ever subscribed
    /// to this fingerprint and nothing changed it before: the channel is minted lazily.
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

    /// The paired clients (for the management API's device list).
    pub fn list(&self) -> Vec<PairedClient> {
        self.store.list()
    }

    /// Remove a paired client by fingerprint. Returns whether one was removed. On a persist
    /// failure the in-memory store is rolled back (it never diverges from disk). A removal
    /// publishes the terminal `revoked` state so the device's live sessions can end themselves
    /// (design §5.6 — unpair reaches every live session within one event).
    pub fn remove(&self, fp_hex: &str) -> Result<bool> {
        let removed = self.store.remove(fp_hex)?;
        if removed {
            self.publish_current(fp_hex);
        }
        Ok(removed)
    }

    /// Remove EVERY paired client in one persisted write. Returns the fingerprints removed, so the
    /// caller can end the sessions they own. On a persist failure nothing is removed. Publishes
    /// `revoked` for each removed fingerprint, like [`Self::remove`].
    pub fn remove_all(&self) -> Result<Vec<String>> {
        let removed = self.store.remove_all()?;
        for fp in &removed {
            self.publish_current(fp);
        }
        Ok(removed)
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
    /// it, else the knock's own name) and drop it from the queue. `access` is the approve dialog's
    /// choice — one of the three authorized grant paths (design §5.7); `None` keeps a re-approved
    /// device's existing access, or the full/permanent default for a first pairing. `Ok(None)` = no
    /// such (or expired) id. Reads (does NOT pre-remove) the entry, then [`Self::add_with_access`]
    /// pins the fingerprint and clears the pending entry — an order a parked waiter relies on (see
    /// [`Self::wait_for_decision`]). Returns the stored record, access fields included.
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
        self.add_with_access(&name, &fp_hex, access)?; // pins, clears the entry, notifies waiters

        // Read the record back rather than assembling it here: for `access == None` on a re-pair
        // the store kept the device's previous grants/expiry, and the caller (the mgmt approve
        // response) must see what is actually in force, not this call's inputs.
        Ok(self.store.get(&fp_hex))
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
    /// The store-blind approval queue is handed a paired-check closure so it can resolve
    /// [`PairingDecision::Approved`] the instant the fingerprint pairs. See
    /// [`approval::ApprovalQueue::wait_for_decision`] for the full decision contract.
    ///
    /// The closure answers with [`Self::effective`], NOT [`Self::is_paired`]: only a knock from
    /// an *unauthorized* device ever parks here, and an EXPIRED guest's record is still *listed*
    /// — resolving on the listing would "admit" its knock instantly, before the operator
    /// re-grants, and the session would then fail admission's own effective-check with a typed
    /// expiry close. Parking until the record is effective again makes re-approval (which
    /// refreshes access) — or a console re-grant — the thing that admits, per design §4.
    /// (Deliberate corollary: a bare PIN re-pair, which preserves an expired record's access
    /// per §5.7, does not admit a parked knock — the device stays unauthorized either way.)
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
        np.approve_pending(id, Some("Approved"), None)
            .unwrap()
            .unwrap();
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

    /// An EXPIRED record's knock must PARK (design §4): the record is still *listed*, and a
    /// paired-check that resolved on the listing would "admit" the knock instantly, before any
    /// re-grant — the session then just dies on admission's effective-check. Only the operator's
    /// re-approval (which refreshes access) may resolve the waiter.
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

        // No re-grant → the waiter times out; the stale listing must not admit it.
        let seq = np.note_pending("Old Guest", "aa77", None);
        let d = np
            .wait_for_decision("aa77", seq, Duration::from_millis(120))
            .await;
        assert_eq!(d, PairingDecision::TimedOut);

        // Re-approval with fresh access resolves a parked waiter — re-approval IS the re-grant.
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
        np.approve_pending(id, None, None).unwrap().unwrap();
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

    fn wall_now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// A store written before grants existed (name + fingerprint only) must decode unchanged and
    /// mean what it always meant: full control, forever.
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

    /// THE security property of WP2 (design §5.7, plan §8 risk table): re-running the pairing
    /// ceremony must never widen access. `add()` — the ceremony choke point when no operator
    /// choice is in play — is name-only for an existing fingerprint. If this test fails, a guest
    /// limited to Controller · tonight can re-pair itself back to full control, forever.
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

        // The guest re-pairs (new ceremony, no operator access choice): name updates, NOTHING else.
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

        // And the limitation survives a restart (it's the persisted record, not memory).
        drop(np);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        assert_eq!(np.effective("aa11", now), Some(GRANT_GAMEPAD));
        let _ = std::fs::remove_file(&p);
    }

    /// The authorized paths DO set access: `add_with_access(Some)` on the ceremony, and the
    /// approve dialog's choice through `approve_pending` — whose return value reports what is
    /// actually stored.
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

    /// Expiry ends *authorization*, not *listing*: `effective()` flips to `None` at the deadline
    /// while `is_paired()`/`list()` keep the row (the console shows "Expired"; re-grant is one
    /// click, not a mysterious disappearance — design §4).
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

    /// A store edited by a future host version (or by hand) can't smuggle reserved bits into this
    /// version's enforcement: `effective()` and the watch state mask with GRANT_ALL on read.
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
        // Editing access is not a way to pair a device.
        assert!(!np.set_access("nope99", from_the_future).unwrap());
        assert!(!np.is_paired("nope99"));
        let _ = std::fs::remove_file(&p);
    }

    /// The watch registry: an edit reaches a live subscriber within one event, and unpair
    /// publishes the terminal `revoked` state (design §5.6). Re-pairing publishes fresh state on
    /// the same channel.
    #[tokio::test]
    async fn watch_publishes_on_set_access_and_unpair() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        np.add("Living Room", "ee55").unwrap();
        // Subscribe under a different hex case — the registry keys case-insensitively, like the
        // store.
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

        // Unpair: the terminal publish a live session ends itself on.
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

        // Re-pairing revives the SAME channel — a stale-but-alive subscriber sees the new state.
        np.add("Living Room", "ee55").unwrap();
        rx.changed().await.unwrap();
        assert!(!rx.borrow().revoked);
        assert_eq!(rx.borrow().grants, GRANT_ALL);

        // Subscribing to a never-paired fingerprint starts revoked.
        assert!(np.subscribe("zz99").borrow().revoked);
        let _ = std::fs::remove_file(&p);
    }

    /// The armed window carries the operator's access choice to whichever device completes the
    /// ceremony; disarm (the single-use consume) wipes it with the PIN.
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
