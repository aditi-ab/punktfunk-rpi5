//! Shared native (`punktfunk/1`) pairing state — the on-demand arming PIN (with expiry) plus the
//! persistent paired-clients store. One [`NativePairing`] handle is shared by the punktfunk/1 QUIC
//! accept loop ([`crate::m3`]) and the management API ([`crate::mgmt`]), so an operator can **arm
//! pairing and read the PIN from the web console** instead of the service log.
//!
//! The PIN direction is inherent to the SPAKE2 ceremony: the *host* mints the PIN and the *client*
//! enters it (the client needs it to build its first message). So the UI **displays** the PIN —
//! armed on demand for a short window — rather than accepting one.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
#[derive(Default)]
struct Armed {
    pin: Option<String>,
    expires_at: Option<Instant>,
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

/// Pending knocks older than this are dropped (the device retries; a stale entry shouldn't be
/// approvable days later when the operator no longer remembers the context).
const PENDING_TTL: Duration = Duration::from_secs(10 * 60);
/// Cap on the pending list — a LAN scanner must not grow it unboundedly. Oldest entries drop.
const PENDING_CAP: usize = 32;

/// Shared native-pairing state: the arming PIN window + the persistent trust store + the
/// pending-approval queue.
pub struct NativePairing {
    arm: Mutex<Armed>,
    paired: Mutex<PairedState>,
    pending: Mutex<PendingState>,
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
    let home = std::env::var("HOME").context("HOME unset")?;
    Ok(PathBuf::from(home).join(".config/punktfunk/punktfunk1-paired.json"))
}

fn load(path: &std::path::Path) -> PairedClients {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(state: &PairedState) -> Result<()> {
    if let Some(dir) = state.path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Atomic replace: a crash/full-disk mid-write must not truncate the trust store (which would
    // silently lock out every paired client on a --require-pairing host). Temp + rename.
    let tmp = state.path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&state.clients)?)?;
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
            }
        } else {
            Armed::default()
        };
        Ok(NativePairing {
            arm: Mutex::new(arm),
            paired: Mutex::new(PairedState { path, clients }),
            pending: Mutex::new(PendingState::default()),
        })
    }

    /// Arm pairing with a fresh random PIN, valid for `ttl`. Returns the PIN to display.
    pub fn arm(&self, ttl: Duration) -> String {
        let pin = random_pin();
        *self.arm.lock().unwrap() = Armed {
            pin: Some(pin.clone()),
            expires_at: Some(Instant::now() + ttl),
        };
        pin
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
        let mut pending = self.pending.lock().unwrap();
        pending
            .items
            .retain(|p| !p.fp_hex.eq_ignore_ascii_case(fp_hex));
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

    /// Record an unpaired device's knock for delegated approval. Re-knocks from the same
    /// fingerprint refresh the existing entry in place (same id; a connect-retry loop must not spam
    /// the list); a fresh fingerprint gets a new id, evicting the **least-recently-active** entry
    /// past [`PENDING_CAP`]. The name is sanitized (untrusted; see [`sanitize_device_name`]).
    pub fn note_pending(&self, name: &str, fp_hex: &str) {
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
            return;
        }
        if pending.items.len() >= PENDING_CAP {
            // Evict the least-recently-active entry. NOT index 0: the in-place refresh above means
            // Vec order no longer tracks recency, so pick the minimum `requested_at` explicitly.
            if let Some(at) = pending
                .items
                .iter()
                .enumerate()
                .min_by_key(|(_, p)| p.requested_at)
                .map(|(i, _)| i)
            {
                pending.items.remove(at);
            }
        }
        let id = pending.next_id;
        pending.next_id = pending.next_id.wrapping_add(1);
        pending.items.push(Pending {
            id,
            name,
            fp_hex: fp_hex.to_string(),
            requested_at: Instant::now(),
        });
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

    /// Approve a pending knock: pair its fingerprint (under `name_override` if the operator
    /// labeled it, else the knock's own name) and drop it from the queue. `Ok(None)` = no such
    /// (or expired) id.
    pub fn approve_pending(
        &self,
        id: u32,
        name_override: Option<&str>,
    ) -> Result<Option<PairedClient>> {
        let entry = {
            let mut pending = self.pending.lock().unwrap();
            Self::expire_pending(&mut pending);
            let Some(at) = pending.items.iter().position(|p| p.id == id) else {
                return Ok(None);
            };
            pending.items.remove(at)
        }; // pending lock released — add() takes the paired lock
        let name = name_override.unwrap_or(&entry.name);
        self.add(name, &entry.fp_hex)?;
        Ok(Some(PairedClient {
            name: name.to_string(),
            fingerprint: entry.fp_hex,
        }))
    }

    /// Deny (drop) a pending knock. Returns whether one was removed. The device's next knock
    /// re-creates an entry — deny is "not now", not a blocklist.
    pub fn deny_pending(&self, id: u32) -> bool {
        let mut pending = self.pending.lock().unwrap();
        let before = pending.items.len();
        pending.items.retain(|p| p.id != id);
        pending.items.len() != before
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
        np.note_pending("device aa11", "AA11");
        np.note_pending("Bedroom TV", "aa11");
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
        np.note_pending("device bb22", "BB22");
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
        for i in 0..(PENDING_CAP + 3) {
            np.note_pending("flood", &format!("f{i:03}"));
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
        np.note_pending("Knocker", "cc44");
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
}
