//! The persistent native-pairing trust store: `~/.config/punktfunk/punktfunk1-paired.json`
//! (plan §W5 — carved out of the [`super`] facade). Owns the paired-clients [`Mutex`] and the
//! atomic-replace persistence; the pending-approval side of a pairing lives in [`super::approval`].

use anyhow::Result;
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

    /// Is this client (hex SHA-256 fingerprint) in the paired set?
    pub(super) fn is_paired(&self, fp_hex: &str) -> bool {
        self.paired.lock().unwrap().clients.contains(fp_hex)
    }

    /// Record a successful pairing (re-pairing the same fingerprint just updates the name —
    /// matched case-insensitively, like every other fingerprint comparison here). The name is
    /// sanitized (untrusted). On a persist failure the in-memory store is rolled back so it never
    /// diverges from disk. (Clearing any pending knock for this fingerprint is the caller's job —
    /// see [`super::approval::ApprovalQueue::admit_and_clear`].)
    pub(super) fn add(&self, name: &str, fp_hex: &str) -> Result<()> {
        let name = super::sanitize_device_name(name, fp_hex);
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
        Ok(())
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
