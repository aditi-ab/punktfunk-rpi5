//! The persistent native-pairing trust store: `~/.config/punktfunk/punktfunk1-paired.json`
//! (plan §W5 — carved out of the [`super`] facade). Owns the paired-clients [`Mutex`] and the
//! atomic-replace persistence; the pending-approval side of a pairing lives in [`super::approval`].

use anyhow::Result;
use punktfunk_core::quic::GRANT_ALL;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
    /// Grant bitmask (`punktfunk_core::quic::GRANT_*`). `None` (absent in stores from before
    /// grants existed) = full control — existing pairings keep today's behavior. Stored as
    /// written; readers mask with [`GRANT_ALL`] so a store edited by a *future* host version
    /// can't smuggle reserved bits into this one's enforcement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grants: Option<u32>,
    /// Absolute expiry, host wall clock, unix seconds. `None` = permanent. Deliberately wall
    /// clock (design §4): the user's mental model is "until tonight", so an NTP step moves the
    /// deadline with the clock; evaluate at each check, never against a cached monotonic offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_unix: Option<i64>,
    /// When the access was granted (unix seconds) — display/audit only, never enforced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_unix: Option<i64>,
}

/// An operator's access choice for a device: what it may do, and for how long. The payload of
/// the authorized-widening paths (arm dialog / approve dialog / console edit — design §5.7);
/// everywhere it is `Option<Access>`, `None` means "no explicit choice" — new records get the
/// full/permanent default and existing records keep what they have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    /// Grant bitmask (`punktfunk_core::quic::GRANT_*` bits). Reserved bits are the management
    /// API's job to reject (400, never silently cleared); the store masks them on *read*.
    pub grants: u32,
    /// Absolute expiry, host wall clock, unix seconds. `None` = permanent.
    pub expires_unix: Option<i64>,
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

fn default_path() -> Result<PathBuf> {
    // `config_dir()` resolves XDG/HOME on Linux and falls back to %APPDATA% on Windows — so the
    // native paired-store works without a HOME env var (which a Windows service/task doesn't set).
    Ok(pf_paths::config_dir().join("punktfunk1-paired.json"))
}

/// Host wall clock, unix seconds — the clock every grant/expiry field is expressed in
/// (design §4: wall time on purpose; the host clock is operator-owned).
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn load(path: &Path) -> PairedClients {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(state: &PairedState) -> Result<()> {
    if let Some(dir) = state.path.parent() {
        pf_paths::create_private_dir(dir)?;
    }
    // Atomic replace: a crash/full-disk mid-write must not truncate the trust store (which would
    // silently lock out every paired client on a --require-pairing host). Temp + rename. The temp is
    // written owner-only so a local user can't inject a fingerprint to pair themselves.
    let tmp = state.path.with_extension("json.tmp");
    pf_paths::write_secret_file(&tmp, &serde_json::to_vec_pretty(&state.clients)?)?;
    std::fs::rename(&tmp, &state.path)?;
    Ok(())
}

/// The persistent trust store — the paired-clients set behind a [`Mutex`], backed by an
/// atomic-replace JSON file.
pub(super) struct TrustStore {
    paired: Mutex<PairedState>,
}

impl TrustStore {
    /// Open (load) the trust store. `store_path = None` uses the default config path.
    pub(super) fn open(store_path: Option<PathBuf>) -> Result<TrustStore> {
        let path = match store_path {
            Some(p) => p,
            None => default_path()?,
        };
        let clients = load(&path);
        Ok(TrustStore {
            paired: Mutex::new(PairedState { path, clients }),
        })
    }

    /// Is this client (hex SHA-256 fingerprint) in the paired set? **Expiry-blind**: an expired
    /// record still answers `true` (it is *listed*, just not authorized) — see
    /// [`Self::effective`] for the authorization question.
    pub(super) fn is_paired(&self, fp_hex: &str) -> bool {
        self.paired.lock().unwrap().clients.contains(fp_hex)
    }

    /// The grant mask this fingerprint is authorized for *right now* — `None` when unpaired OR
    /// expired, `Some(mask)` otherwise (absent grants = [`GRANT_ALL`], the pre-grants record).
    /// The mask is ANDed with [`GRANT_ALL`] on the way out: a store written by a future host
    /// version (or hand-edited) can't smuggle reserved bits into this version's enforcement.
    /// `now_unix` is the caller's wall clock — passed in, not sampled here, so the expiry
    /// evaluation and whatever decision it feeds share one instant.
    pub(super) fn effective(&self, fp_hex: &str, now_unix: i64) -> Option<u32> {
        let p = self.paired.lock().unwrap();
        let c = p
            .clients
            .clients
            .iter()
            .find(|c| c.fingerprint.eq_ignore_ascii_case(fp_hex))?;
        if c.expires_unix.is_some_and(|t| now_unix >= t) {
            return None;
        }
        Some(c.grants.unwrap_or(GRANT_ALL) & GRANT_ALL)
    }

    /// The stored record for a fingerprint (for the facade's watch-state snapshot and the
    /// approval path's honest return value). Verbatim — no expiry evaluation, no masking.
    pub(super) fn get(&self, fp_hex: &str) -> Option<PairedClient> {
        self.paired
            .lock()
            .unwrap()
            .clients
            .clients
            .iter()
            .find(|c| c.fingerprint.eq_ignore_ascii_case(fp_hex))
            .cloned()
    }

    /// Record a successful pairing with no explicit access choice. For a **new** fingerprint this
    /// mints the legacy full/permanent record (all access fields absent — byte-identical to a
    /// pre-grants store). For an **existing** fingerprint it is name-only: grants, expiry, and
    /// granted-time are preserved. That asymmetry is the security property (design §5.7): today a
    /// guest limited to Controller · 4 h could re-run the pairing ceremony and this method used to
    /// *replace* the record — silently escalating to full control. The only paths that widen
    /// access take an explicit [`Access`] via [`Self::add_with_access`] / [`Self::set_access`],
    /// and all of them sit behind the operator (mgmt bearer / armed window).
    pub(super) fn add(&self, name: &str, fp_hex: &str) -> Result<()> {
        self.add_with_access(name, fp_hex, None)
    }

    /// Record a successful pairing, optionally with the operator's access choice. `Some(access)`
    /// (the arm/approve dialogs) sets grants + expiry and stamps `granted_unix` — on a re-pair it
    /// *replaces* the previous access, because the operator just chose anew. `None` behaves like
    /// [`Self::add`] (name-only for an existing fingerprint; full/permanent default for a new
    /// one). The fingerprint match is case-insensitive, like every other comparison here; the
    /// name is sanitized (untrusted). On a persist failure the in-memory store is rolled back so
    /// it never diverges from disk. (Clearing any pending knock for this fingerprint is the
    /// caller's job — see [`super::approval::ApprovalQueue::admit_and_clear`].)
    pub(super) fn add_with_access(
        &self,
        name: &str,
        fp_hex: &str,
        access: Option<Access>,
    ) -> Result<()> {
        let name = super::sanitize_device_name(name, fp_hex);
        let mut p = self.paired.lock().unwrap();
        let snapshot = p.clients.clients.clone(); // restore on a failed save
        match p
            .clients
            .clients
            .iter_mut()
            .find(|c| c.fingerprint.eq_ignore_ascii_case(fp_hex))
        {
            Some(existing) => {
                existing.name = name;
                if let Some(a) = access {
                    existing.grants = Some(a.grants);
                    existing.expires_unix = a.expires_unix;
                    existing.granted_unix = Some(now_unix());
                }
            }
            None => p.clients.clients.push(PairedClient {
                name,
                fingerprint: fp_hex.to_string(),
                grants: access.map(|a| a.grants),
                expires_unix: access.and_then(|a| a.expires_unix),
                granted_unix: access.map(|_| now_unix()),
            }),
        }
        if let Err(e) = save(&p) {
            p.clients.clients = snapshot;
            return Err(e);
        }
        Ok(())
    }

    /// Overwrite an existing record's access (the console edit sheet / "expire now" / extend).
    /// Returns `false` (and writes nothing) for an unknown fingerprint — editing access is not a
    /// way to pair a device. On a persist failure the in-memory store is rolled back.
    pub(super) fn set_access(&self, fp_hex: &str, access: Access) -> Result<bool> {
        let mut p = self.paired.lock().unwrap();
        let snapshot = p.clients.clients.clone();
        let Some(existing) = p
            .clients
            .clients
            .iter_mut()
            .find(|c| c.fingerprint.eq_ignore_ascii_case(fp_hex))
        else {
            return Ok(false);
        };
        existing.grants = Some(access.grants);
        existing.expires_unix = access.expires_unix;
        existing.granted_unix = Some(now_unix());
        if let Err(e) = save(&p) {
            p.clients.clients = snapshot;
            return Err(e);
        }
        Ok(true)
    }

    /// The paired clients (for the management API's device list).
    pub(super) fn list(&self) -> Vec<PairedClient> {
        self.paired.lock().unwrap().clients.clients.clone()
    }

    /// Remove a paired client by fingerprint. Returns whether one was removed. On a persist
    /// failure the in-memory store is rolled back (it never diverges from disk).
    pub(super) fn remove(&self, fp_hex: &str) -> Result<bool> {
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

    /// Remove EVERY paired client, in ONE persisted write. Returns the fingerprints removed, so
    /// the caller can tear down the live sessions they own. On a persist failure the in-memory
    /// store is rolled back (it never diverges from disk), exactly like [`Self::remove`].
    ///
    /// Not a loop over [`Self::remove`]: that would rewrite (and fsync-rename) the store once per
    /// client, and a failure partway would leave the operator with a half-emptied trust store and
    /// no way to tell which half.
    pub(super) fn remove_all(&self) -> Result<Vec<String>> {
        let mut p = self.paired.lock().unwrap();
        if p.clients.clients.is_empty() {
            return Ok(Vec::new());
        }
        // `take` leaves the empty list in place to be persisted, and hands us the snapshot that
        // doubles as both the rollback value and the removed-fingerprint report.
        let snapshot = std::mem::take(&mut p.clients.clients);
        if let Err(e) = save(&p) {
            p.clients.clients = snapshot;
            return Err(e);
        }
        Ok(snapshot.into_iter().map(|c| c.fingerprint).collect())
    }

    /// The number of paired clients (for the status snapshot).
    pub(super) fn count(&self) -> u32 {
        self.paired.lock().unwrap().clients.clients.len() as u32
    }
}
