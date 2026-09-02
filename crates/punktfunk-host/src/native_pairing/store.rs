//! Persistent paired-client store: `punktfunk1-paired.json` under the config dir.
//!
//! Owns the paired-clients [`Mutex`] and atomic-replace persistence. GameStream
//! pairing is a separate store. Pending knocks live in [`super::approval`].
//! Persist failures roll back in-memory state so RAM never diverges from disk.
//!
//! Pin this via [`TrustStore`]. Grant masks are stored as written; readers AND
//! with [`GRANT_ALL`]. Expiry is host wall clock, re-evaluated at each check.

use anyhow::Result;
use punktfunk_core::quic::GRANT_ALL;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Paired punktfunk/1 clients. GameStream pairing uses a different store.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct PairedClients {
    pub clients: Vec<PairedClient>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct PairedClient {
    pub name: String,
    /// Hex SHA-256 of the client's certificate.
    pub fingerprint: String,
    /// `GRANT_*` mask. `None` (pre-grants stores) is full control. Readers AND with
    /// [`GRANT_ALL`] so reserved bits from a newer writer cannot take effect here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grants: Option<u32>,
    /// Host wall-clock unix seconds. `None` is permanent. Re-evaluate at each check;
    /// do not cache against a monotonic offset — an NTP step must move the deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_unix: Option<i64>,
    /// Grant time, unix seconds. Display/audit only; never enforced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_unix: Option<i64>,
}

/// Operator access choice. `Option<Access>::None` means no choice: new records get
/// full/permanent, existing records keep their grants and expiry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    /// `GRANT_*` bits. The store masks reserved bits on read; the management API
    /// must 400 them — never silently clear.
    pub grants: u32,
    /// Host wall-clock unix seconds. `None` is permanent.
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
    // `config_dir()` falls back to %APPDATA% when HOME is unset (Windows service).
    Ok(pf_paths::config_dir().join("punktfunk1-paired.json"))
}

/// Host wall-clock unix seconds. Grant and expiry fields use this clock.
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
    // Temp + rename so a crash mid-write cannot truncate the store. Owner-only so a
    // local user cannot inject a fingerprint.
    let tmp = state.path.with_extension("json.tmp");
    pf_paths::write_secret_file(&tmp, &serde_json::to_vec_pretty(&state.clients)?)?;
    std::fs::rename(&tmp, &state.path)?;
    Ok(())
}

pub(super) struct TrustStore {
    paired: Mutex<PairedState>,
}

impl TrustStore {
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

    /// Present in the store, including expired records. Use [`Self::effective`] for authorization.
    pub(super) fn is_paired(&self, fp_hex: &str) -> bool {
        self.paired.lock().unwrap().clients.contains(fp_hex)
    }

    /// Authorized mask right now: `None` if unpaired or expired. Absent grants are
    /// [`GRANT_ALL`]. AND with [`GRANT_ALL`] on the way out; `normalize_legacy_full`
    /// maps the old full-control value. `now_unix` is the caller's clock so expiry
    /// and the decision it feeds share one instant.
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
        Some(punktfunk_core::quic::normalize_legacy_full(c.grants.unwrap_or(GRANT_ALL)) & GRANT_ALL)
    }

    /// Stored record, verbatim: no expiry check and no grant mask.
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

    /// Pair with no access choice. A new fingerprint gets full/permanent; an existing
    /// one updates the name only. Widening grants requires [`Self::add_with_access`]
    /// or [`Self::set_access`].
    pub(super) fn add(&self, name: &str, fp_hex: &str) -> Result<()> {
        self.add_with_access(name, fp_hex, None)
    }

    /// Pair, optionally with [`Access`]. `Some` replaces grants and expiry; `None`
    /// matches [`Self::add`]. Persist failure rolls back RAM. The caller clears any
    /// pending knock ([`super::approval::ApprovalQueue::admit_and_clear`]).
    pub(super) fn add_with_access(
        &self,
        name: &str,
        fp_hex: &str,
        access: Option<Access>,
    ) -> Result<()> {
        let name = super::sanitize_device_name(name, fp_hex);
        let mut p = self.paired.lock().unwrap();
        let snapshot = p.clients.clients.clone(); // rollback if save fails
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

    /// Overwrite access on an existing record. Unknown fingerprint returns `false`
    /// and writes nothing — this is not a pairing path. Persist failure rolls back RAM.
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

    pub(super) fn list(&self) -> Vec<PairedClient> {
        self.paired.lock().unwrap().clients.clients.clone()
    }

    /// Drop this fingerprint. Persist failure rolls back RAM so it matches disk.
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

    /// Drop every client in one persist. Returns the removed fingerprints so live
    /// sessions can be torn down. Not a loop over [`Self::remove`]: a mid-loop
    /// failure would leave a half-emptied store. Persist failure rolls back RAM.
    pub(super) fn remove_all(&self) -> Result<Vec<String>> {
        let mut p = self.paired.lock().unwrap();
        if p.clients.clients.is_empty() {
            return Ok(Vec::new());
        }
        // Empty list is what we persist; the taken vec is rollback and the return value.
        let snapshot = std::mem::take(&mut p.clients.clients);
        if let Err(e) = save(&p) {
            p.clients.clients = snapshot;
            return Err(e);
        }
        Ok(snapshot.into_iter().map(|c| c.fingerprint).collect())
    }

    pub(super) fn count(&self) -> u32 {
        self.paired.lock().unwrap().clients.clients.len() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_store_files_open_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cases: [&[u8]; 3] = [b"", br#"{"clients":["#, b"not json"];

        for (index, contents) in cases.into_iter().enumerate() {
            let path = dir.path().join(format!("{index}.json"));
            std::fs::write(&path, contents).unwrap();

            let store = TrustStore::open(Some(path)).unwrap();

            assert_eq!(store.count(), 0);
            assert!(store.list().is_empty());
        }
    }
}
