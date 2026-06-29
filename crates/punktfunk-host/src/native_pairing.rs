//! Shared native (`punktfunk/1`) pairing state — the on-demand arming PIN (with expiry) plus the
//! persistent paired-clients store. One [`NativePairing`] handle is shared by the punktfunk/1 QUIC
//! accept loop ([`crate::punktfunk1`]) and the management API ([`crate::mgmt`]), so an operator can **arm
//! pairing and read the PIN from the web console** instead of the service log.
//!
//! The PIN direction is inherent to the SPAKE2 ceremony: the *host* mints the PIN and the *client*
//! enters it (the client needs it to build its first message). So the UI **displays** the PIN —
//! armed on demand for a short window — rather than accepting one.

use anyhow::Result;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// The host's paired punktfunk/1 clients: `~/.config/punktfunk/punktfunk1-paired.json`.
/// (Separate from GameStream pairing, which has its own store and ceremony.)
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct PairedClients {
    pub clients: Vec<PairedClient>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct PairedClient {
    pub name: String,
    /// Hex SHA-256 of the client's certificate.
    pub fingerprint: String,
}

impl PairedClients {
    fn contains(&self, fp_hex: &str) -> bool {
        self.clients
            .iter()
            .any(|c| c.fingerprint.eq_ignore_ascii_case(fp_hex))
    }
}

struct PairedState {
    path: PathBuf,
    clients: PairedClients,
}

/// The current arming window. `pin == None` ⇒ disarmed. `expires_at == None` ⇒ armed with no
/// expiry (the CLI `--allow-pairing` flag); `Some(t)` ⇒ a web-armed window that auto-disarms.
///
/// `bound_fp == Some(fp)` ⇒ the window is **bound to one operator-selected device fingerprint**:
/// only a pairing attempt from that fingerprint may consume it (security-review 2026-06-28 #9). This
/// closes the window-burn DoS — an unpaired LAN peer cannot consume a window armed for a specific
/// device, because the QUIC client-auth proves cert possession (it can't forge the bound fingerprint).
/// `None` ⇒ unbound (the CLI flag / a console "arm open"): any well-formed attempt consumes it (the
/// legacy behavior, retaining the window-burn DoS — acceptable only on a trusted LAN).
#[derive(Default)]
struct Armed {
    pin: Option<String>,
    expires_at: Option<Instant>,
    bound_fp: Option<String>,
}

/// The result of resolving the armed PIN for a specific client fingerprint ([`NativePairing::pin_for_attempt`]).
pub enum PinAttempt {
    /// No window is armed (disarmed/expired) — reject; do not run the ceremony.
    Disarmed,
    /// A window IS armed but **bound to a different fingerprint** — reject WITHOUT consuming it, so
    /// an unrelated (attacker) fingerprint can't burn the operator's armed window (#9).
    BoundToOther,
    /// Proceed: the PIN to run the ceremony with (the window is unbound, or bound to this fingerprint).
    Pin(String),
}

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
    /// True while a connection is held open in [`NativePairing::wait_for_decision`] for this knock.
    /// A live parked knock is a genuine device waiting for the operator — eviction skips it unless
    /// every entry is parked, so a cert-rotating flood can't evict the device being onboarded (#13).
    parked: bool,
}

#[derive(Default)]
struct PendingState {
    next_id: u32,
    items: Vec<Pending>,
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

/// The outcome of [`NativePairing::wait_for_decision`] — what an operator did with a parked,
/// unpaired knock (delegated approval, roadmap §8b-1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairingDecision {
    /// The operator clicked Approve (the fingerprint is now paired) — admit the session.
    Approved,
    /// The operator denied, or the pending entry was otherwise dropped without pairing — reject.
    Denied,
    /// No decision within the wait window — reject; the device can knock again.
    TimedOut,
}

/// Pending knocks older than this are dropped (the device retries; a stale entry shouldn't be
/// approvable days later when the operator no longer remembers the context).
const PENDING_TTL: Duration = Duration::from_secs(10 * 60);
/// Cap on the pending list — a LAN scanner must not grow it unboundedly. Oldest entries drop.
const PENDING_CAP: usize = 32;
/// Max pending knocks one source IP may occupy, so a single host can't fill the whole queue and hide
/// / evict a genuine device's knock (security-review 2026-06-28 #13). The QUIC path is address-
/// validated, so the source IP isn't off-path spoofable; an attacker would need that many real hosts.
const MAX_PENDING_PER_IP: usize = 4;

/// Shared native-pairing state: the arming PIN window + the persistent trust store + the
/// pending-approval queue.
pub struct NativePairing {
    arm: Mutex<Armed>,
    paired: Mutex<PairedState>,
    pending: Mutex<PendingState>,
    /// Notified whenever the trust/pending state changes (a fingerprint paired, or a pending knock
    /// denied/dropped), so a QUIC connection parked in [`NativePairing::wait_for_decision`] wakes
    /// the instant an operator acts in the console — the substrate for delegated approval admitting
    /// a session with no client reconnect.
    changed: Notify,
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

fn default_path() -> Result<PathBuf> {
    // `config_dir()` resolves XDG/HOME on Linux and falls back to %APPDATA% on Windows — so the
    // native paired-store works without a HOME env var (which a Windows service/task doesn't set).
    Ok(crate::gamestream::config_dir().join("punktfunk1-paired.json"))
}

fn load(path: &std::path::Path) -> PairedClients {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(state: &PairedState) -> Result<()> {
    if let Some(dir) = state.path.parent() {
        crate::gamestream::create_private_dir(dir)?;
    }
    // Atomic replace: a crash/full-disk mid-write must not truncate the trust store (which would
    // silently lock out every paired client on a --require-pairing host). Temp + rename. The temp is
    // written owner-only so a local user can't inject a fingerprint to pair themselves.
    let tmp = state.path.with_extension("json.tmp");
    crate::gamestream::write_secret_file(&tmp, &serde_json::to_vec_pretty(&state.clients)?)?;
    std::fs::rename(&tmp, &state.path)?;
    Ok(())
}

fn random_pin() -> String {
    use rand::Rng;
    format!("{:04}", rand::thread_rng().gen_range(0..10_000u32))
}

/// Sanitize a client-supplied device name before it's stored, listed, or logged. The name comes
/// straight off the wire (the `Hello`/`PairRequest` of an *unpaired* device), so it's untrusted: a
/// hostile LAN device could embed terminal escapes / control characters (log + console injection) or
/// bidi overrides (`U+202E` etc.) to make a malicious device *look* like a trusted one in the
/// approval UI. Strip C0/C1 controls and Unicode bidi/format controls, collapse whitespace, trim, and
/// cap the length; an empty/all-control name falls back to a fingerprint-derived label.
pub(crate) fn sanitize_device_name(name: &str, fp_hex: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '\t' || c == '\n' { ' ' } else { c })
        .filter(|&c| {
            !c.is_control()
                // Bidi/format controls that could spoof or reorder the displayed name.
                && !('\u{202A}'..='\u{202E}').contains(&c) // LRE..RLO/PDF
                && !('\u{2066}'..='\u{2069}').contains(&c) // LRI..PDI
                && c != '\u{200E}' // LRM
                && c != '\u{200F}' // RLM
                && c != '\u{061C}' // ALM
                && c != '\u{FEFF}' // BOM / zero-width no-break space
        })
        .collect();
    // Collapse internal whitespace runs, trim, cap at the wire limit.
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut trimmed = collapsed.as_str();
    while trimmed.len() > NAME_MAX {
        let mut cut = NAME_MAX;
        while !trimmed.is_char_boundary(cut) {
            cut -= 1;
        }
        trimmed = &trimmed[..cut];
    }
    let trimmed = trimmed.trim();
    if trimmed.is_empty() {
        format!("device {}", &fp_hex[..8.min(fp_hex.len())])
    } else {
        trimmed.to_string()
    }
}

/// Max stored device-name length (matches the `Hello` wire cap, `quic::HELLO_NAME_MAX`).
const NAME_MAX: usize = 64;

impl NativePairing {
    /// Load the trust store. `store_path = None` uses the default config path. If `arm_at_start`
    /// (the CLI `--allow-pairing`/`--require-pairing` flags), arm immediately with `fixed_pin`
    /// (or a fresh random PIN) and **no expiry** — back-compat with the headless CLI flow.
    pub fn load_with(
        store_path: Option<PathBuf>,
        fixed_pin: Option<String>,
        arm_at_start: bool,
    ) -> Result<NativePairing> {
        let path = match store_path {
            Some(p) => p,
            None => default_path()?,
        };
        let clients = load(&path);
        let arm = if arm_at_start {
            Armed {
                pin: Some(fixed_pin.unwrap_or_else(random_pin)),
                expires_at: None,
                bound_fp: None,
            }
        } else {
            Armed::default()
        };
        Ok(NativePairing {
            arm: Mutex::new(arm),
            paired: Mutex::new(PairedState { path, clients }),
            pending: Mutex::new(PendingState::default()),
            changed: Notify::new(),
        })
    }

    /// Arm pairing with a fresh random PIN, valid for `ttl`, **unbound** (any well-formed attempt
    /// consumes it). Returns the PIN to display. Prefer [`Self::arm_for`] with a specific device
    /// fingerprint on untrusted LANs — an unbound window is burnable by any peer (#9).
    pub fn arm(&self, ttl: Duration) -> String {
        self.arm_for(ttl, None)
    }

    /// Arm pairing with a fresh random PIN, valid for `ttl`. If `bound_fp` is `Some`, the window is
    /// bound to that device fingerprint: only a pairing attempt from it consumes the window, so an
    /// unrelated (attacker) fingerprint can neither pair nor burn the window (#9). Returns the PIN.
    pub fn arm_for(&self, ttl: Duration, bound_fp: Option<String>) -> String {
        let pin = random_pin();
        *self.arm.lock().unwrap() = Armed {
            pin: Some(pin.clone()),
            expires_at: Some(Instant::now() + ttl),
            bound_fp,
        };
        pin
    }

    /// Resolve the PIN for an attempt from `client_fp_hex`, honoring fingerprint binding (#9):
    /// `Disarmed` if no window is armed; `BoundToOther` if a window is armed but bound to a different
    /// fingerprint (the caller MUST reject without consuming it); else `Pin` to run the ceremony.
    pub fn pin_for_attempt(&self, client_fp_hex: &str) -> PinAttempt {
        let mut arm = self.arm.lock().unwrap();
        Self::expire(&mut arm);
        match &arm.pin {
            None => PinAttempt::Disarmed,
            Some(pin) => match &arm.bound_fp {
                Some(bound) if !bound.eq_ignore_ascii_case(client_fp_hex) => PinAttempt::BoundToOther,
                _ => PinAttempt::Pin(pin.clone()),
            },
        }
    }

    /// Disarm pairing (no new ceremonies accepted).
    pub fn disarm(&self) {
        *self.arm.lock().unwrap() = Armed::default();
    }

    /// Expire a timed window if its deadline passed (called under the lock before any read).
    fn expire(arm: &mut Armed) {
        if let Some(t) = arm.expires_at {
            if Instant::now() >= t {
                *arm = Armed::default();
            }
        }
    }

    /// The current valid PIN, or `None` if disarmed/expired. The QUIC ceremony reads this
    /// per-attempt, so a window that lapsed mid-connection no longer pairs.
    pub fn current_pin(&self) -> Option<String> {
        let mut arm = self.arm.lock().unwrap();
        Self::expire(&mut arm);
        arm.pin.clone()
    }

    /// A snapshot for the management API.
    pub fn status(&self) -> NativePairingStatus {
        let mut arm = self.arm.lock().unwrap();
        Self::expire(&mut arm);
        let expires_in_secs = arm
            .expires_at
            .map(|t| t.saturating_duration_since(Instant::now()).as_secs());
        NativePairingStatus {
            armed: arm.pin.is_some(),
            pin: arm.pin.clone(),
            expires_in_secs,
            paired_clients: self.paired.lock().unwrap().clients.clients.len() as u32,
        }
    }

    /// Is this client (hex SHA-256 fingerprint) in the paired set?
    pub fn is_paired(&self, fp_hex: &str) -> bool {
        self.paired.lock().unwrap().clients.contains(fp_hex)
    }

    /// Record a successful pairing (re-pairing the same fingerprint just updates the name —
    /// matched case-insensitively, like every other fingerprint comparison here). The name is
    /// sanitized (untrusted). On a persist failure the in-memory store is rolled back so it never
    /// diverges from disk. Also clears any pending knock for this fingerprint (it's now paired).
    pub fn add(&self, name: &str, fp_hex: &str) -> Result<()> {
        let name = sanitize_device_name(name, fp_hex);
        {
            let mut p = self.paired.lock().unwrap();
            let snapshot = p.clients.clients.clone(); // restore on a failed save
            p.clients
                .clients
                .retain(|c| !c.fingerprint.eq_ignore_ascii_case(fp_hex));
            p.clients.clients.push(PairedClient {
                name,
                fingerprint: fp_hex.to_string(),
            });
            if let Err(e) = save(&p) {
                p.clients.clients = snapshot;
                return Err(e);
            }
        }
        // A device that knocked and is now paired shouldn't linger in the approval list.
        {
            let mut pending = self.pending.lock().unwrap();
            pending
                .items
                .retain(|p| !p.fp_hex.eq_ignore_ascii_case(fp_hex));
        }
        // Wake any connection parked in `wait_for_decision` for this fingerprint: pairing just
        // completed (console approve or the PIN ceremony), so it can admit the session with no
        // reconnect. Notified AFTER the pin AND the pending-clear so a woken waiter observes the
        // fully settled state (paired = true, no longer pending) — see `wait_for_decision`.
        self.changed.notify_waiters();
        Ok(())
    }

    /// The paired clients (for the management API's device list).
    pub fn list(&self) -> Vec<PairedClient> {
        self.paired.lock().unwrap().clients.clients.clone()
    }

    /// Remove a paired client by fingerprint. Returns whether one was removed. On a persist
    /// failure the in-memory store is rolled back (it never diverges from disk).
    pub fn remove(&self, fp_hex: &str) -> Result<bool> {
        let mut p = self.paired.lock().unwrap();
        let before = p.clients.clients.len();
        let snapshot = p.clients.clients.clone();
        p.clients
            .clients
            .retain(|c| !c.fingerprint.eq_ignore_ascii_case(fp_hex));
        let removed = p.clients.clients.len() != before;
        if removed {
            if let Err(e) = save(&p) {
                p.clients.clients = snapshot;
                return Err(e);
            }
        }
        Ok(removed)
    }

    // -- Delegated approval (roadmap §8b-1) --------------------------------

    /// Drop expired pending knocks (called under the lock, mirroring [`Self::expire`]).
    fn expire_pending(pending: &mut PendingState) {
        pending
            .items
            .retain(|p| p.requested_at.elapsed() < PENDING_TTL);
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
    /// refresh the existing entry in place (same id; a connect-retry loop must not spam the list). A
    /// fresh fingerprint gets a new id; the queue is bounded two ways so a flood can't crowd out a
    /// genuine knock (#13): a **per-source-IP cap** ([`MAX_PENDING_PER_IP`]) means one host can hold at
    /// most a few slots, and the global [`PENDING_CAP`] evicts the least-recently-active **non-parked**
    /// entry (never a live, held-open parked knock). The name is sanitized (untrusted).
    pub fn note_pending(&self, name: &str, fp_hex: &str, src_ip: Option<IpAddr>) {
        let name = sanitize_device_name(name, fp_hex);
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
            return;
        }
        // Per-source-IP cap: a single host can't occupy more than MAX_PENDING_PER_IP slots — evict its
        // own oldest entry first so it can't crowd out other devices' knocks (#13).
        if let Some(ip) = src_ip {
            if pending.items.iter().filter(|p| p.src_ip == Some(ip)).count() >= MAX_PENDING_PER_IP {
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
        });
    }

    /// Mark/unmark the pending entry for `fp_hex` as having a live parked waiter (no-op if it's gone).
    /// A parked entry is protected from eviction under load (#13).
    fn set_parked(&self, fp_hex: &str, parked: bool) {
        let mut pending = self.pending.lock().unwrap();
        if let Some(p) = pending
            .items
            .iter_mut()
            .find(|p| p.fp_hex.eq_ignore_ascii_case(fp_hex))
        {
            p.parked = parked;
        }
    }

    /// The devices currently awaiting approval (for the management API).
    pub fn pending(&self) -> Vec<PendingRequest> {
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
    pub fn pending_contains(&self, fp_hex: &str) -> bool {
        let mut pending = self.pending.lock().unwrap();
        Self::expire_pending(&mut pending);
        pending
            .items
            .iter()
            .any(|p| p.fp_hex.eq_ignore_ascii_case(fp_hex))
    }

    /// Approve a pending knock: pair its fingerprint (under `name_override` if the operator
    /// labeled it, else the knock's own name) and drop it from the queue. `Ok(None)` = no such
    /// (or expired) id.
    pub fn approve_pending(
        &self,
        id: u32,
        name_override: Option<&str>,
    ) -> Result<Option<PairedClient>> {
        // Read (do NOT pre-remove) the entry: `add()` pins the fingerprint and THEN clears its
        // pending entry — an order `wait_for_decision` relies on so a parked waiter never observes
        // the device as "neither pending nor paired" (which would read as a denial). Removing here
        // first would open exactly that window.
        let (knock_name, fp_hex) = {
            let mut pending = self.pending.lock().unwrap();
            Self::expire_pending(&mut pending);
            match pending.items.iter().find(|p| p.id == id) {
                Some(p) => (p.name.clone(), p.fp_hex.clone()),
                None => return Ok(None),
            }
        }; // pending lock released — add() takes the paired then pending locks
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
    /// Returns [`PairingDecision::Approved`] the instant the fingerprint is paired (console
    /// approve or a concurrent PIN ceremony), [`PairingDecision::Denied`] if its pending entry is
    /// dropped without pairing, or [`PairingDecision::TimedOut`] if the window lapses. Holds no
    /// lock across the await. The QUIC accept path calls this right after [`Self::note_pending`]
    /// to keep the knocking connection open until a human clicks Approve — so the device pairs and
    /// streams with no reconnect (delegated approval, roadmap §8b-1).
    pub async fn wait_for_decision(&self, fp_hex: &str, timeout: Duration) -> PairingDecision {
        // Mark this knock parked so a cert-rotating flood can't evict the genuine, held-open
        // connection out of the pending queue while the operator decides (#13). Cleared on every
        // exit path by the guard's Drop.
        self.set_parked(fp_hex, true);
        struct ParkGuard<'a> {
            np: &'a NativePairing,
            fp: &'a str,
        }
        impl Drop for ParkGuard<'_> {
            fn drop(&mut self) {
                self.np.set_parked(self.fp, false);
            }
        }
        let _park = ParkGuard { np: self, fp: fp_hex };
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Arm the wakeup BEFORE re-reading state, and `enable()` it, so an approve/deny that
            // lands between the state check and the await still wakes us (no lost notification).
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.is_paired(fp_hex) {
                return PairingDecision::Approved;
            }
            if !self.pending_contains(fp_hex) {
                // Neither pending nor paired. This is almost always a denial — but it can also be
                // the tiny interval inside `add()` between pinning and clearing the pending entry.
                // Re-check `is_paired` once: because `add()` pins BEFORE it clears pending, a
                // cleared-pending observation that is really an approval will now read as paired.
                if self.is_paired(fp_hex) {
                    return PairingDecision::Approved;
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

#[cfg(test)]
mod tests {
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
    fn sanitize_strips_control_and_bidi() {
        // ANSI escape + newline + a bidi override that could spoof the displayed name.
        let dirty = "\u{1b}]0;evil\u{07}Good\nDevice\u{202E}xfp";
        let clean = sanitize_device_name(dirty, "deadbeef00");
        assert!(!clean.contains('\u{1b}') && !clean.contains('\n') && !clean.contains('\u{202E}'));
        // ESC dropped (']' survives), BEL dropped, '\n'→space (Good Device), RLO dropped (no space).
        assert_eq!(clean, "]0;evilGood Devicexfp");
        // All-control / empty → fingerprint-derived fallback.
        assert_eq!(
            sanitize_device_name("\u{1b}\u{07}", "deadbeef00"),
            "device deadbeef"
        );
        assert_eq!(sanitize_device_name("   ", "abc"), "device abc");
        // Over-long names cap at a char boundary.
        assert!(sanitize_device_name(&"x".repeat(200), "ab").len() <= 64);
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
        np.note_pending("Knocker", "ab01", None);
        let d = np
            .wait_for_decision("ab01", Duration::from_millis(80))
            .await;
        assert_eq!(d, PairingDecision::TimedOut);
        assert!(np.pending_contains("ab01"));

        // Approved: approving WHILE parked wakes the waiter with Approved.
        let np2 = np.clone();
        let waiter =
            tokio::spawn(
                async move { np2.wait_for_decision("ab01", Duration::from_secs(5)).await },
            );
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
        np.note_pending("Knock2", "cd02", None);
        let np3 = np.clone();
        let waiter =
            tokio::spawn(
                async move { np3.wait_for_decision("cd02", Duration::from_secs(5)).await },
            );
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

        // Already paired before the call → immediate Approved (no waiting).
        let d = np.wait_for_decision("ab01", Duration::from_secs(5)).await;
        assert_eq!(d, PairingDecision::Approved);
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
        assert!(matches!(np.pin_for_attempt("bb22"), PinAttempt::BoundToOther));
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
        np.note_pending("Living Room", "legit01", Some(legit));
        np.set_parked("legit01", true);
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
