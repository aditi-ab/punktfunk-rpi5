//! Per-client → stable monitor-id map for pf-vdisplay (Phase 2: per-client display-config persistence).
//!
//! Windows keys per-monitor config — notably DPI **scaling** (`HKCU\Control Panel\Desktop\PerMonitorSettings`)
//! — on the monitor's EDID identity AND its OS device path (whose per-connector discriminator is the IddCx
//! `ConnectorIndex` → target UID). The pf-vdisplay driver seeds BOTH the EDID serial and the `ConnectorIndex`
//! from a single monitor `id`. So for Windows to REAPPLY a given client's saved scaling on reconnect, that
//! client must get the SAME `id` every time. This map assigns each client (keyed by its cert fingerprint) a
//! STABLE id and the host passes it as [`AddRequest::preferred_monitor_id`](pf_driver_proto::control::AddRequest).
//!
//! The id space is bounded to `1..=15` because the driver uses the id as the IddCx `ConnectorIndex`, which
//! must stay `< MaxMonitorsSupported` (16). When more than 15 distinct clients are remembered, the
//! LEAST-RECENTLY-USED entry is evicted and its id reused (that evicted client simply re-establishes its
//! scaling once on its next connect). The map persists to `%ProgramData%\punktfunk\pf-vdisplay-identity.json`
//! so ids — and therefore the client→config association — survive host restarts.
//!
//! Anonymous/TOFU and GameStream sessions have no fingerprint and resolve to id `0` (auto) upstream, never
//! reaching this map — they keep the driver's lowest-free slot behavior unchanged.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Max stable id. The driver uses the id as the IddCx `ConnectorIndex`, which must stay
/// `< MaxMonitorsSupported` (16) — so ids run `1..=15`.
const MAX_ID: u32 = 15;

#[derive(Serialize, Deserialize, Default)]
struct Store {
    /// Monotonic most-recently-used counter (the entry with the highest `seen` is the MRU). Persisted so
    /// the LRU ordering survives host restarts.
    tick: u64,
    entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    /// Lower-hex client cert fingerprint (the map key).
    fp: String,
    /// The client's stable monitor id (`1..=15`).
    id: u32,
    /// MRU stamp (compared against [`Store::tick`]).
    seen: u64,
}

/// Persistent fingerprint → stable-id map (see the module docs).
pub(crate) struct MonitorIdentityMap {
    path: PathBuf,
    store: Store,
}

impl MonitorIdentityMap {
    /// Load the persisted map (empty on first run / unreadable / parse failure — a fresh map just
    /// re-derives ids, costing a client one scaling re-set the first time).
    pub(crate) fn load() -> Self {
        let path = crate::gamestream::config_dir().join("pf-vdisplay-identity.json");
        let mut store = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Store>(&b).ok())
            .unwrap_or_default();
        // SANITIZE a hand-edited / corrupt / cross-version file before trusting it: resolve()'s found-entry
        // branch returns the stored id verbatim, so an out-of-range id (0 = the "auto" sentinel, or
        // > MAX_ID) or a duplicate id/fp would flow straight into preferred_monitor_id. Drop out-of-range
        // ids and dedup by BOTH fp and id (keeping the most-recently-seen on a clash) so no two fingerprints
        // can map to the same id. (The driver also rejects a live-colliding id as a backstop.)
        store.entries.sort_by_key(|e| std::cmp::Reverse(e.seen));
        let mut seen_fp = std::collections::HashSet::new();
        let mut seen_id = std::collections::HashSet::new();
        store.entries.retain(|e| {
            (1..=MAX_ID).contains(&e.id) && seen_fp.insert(e.fp.clone()) && seen_id.insert(e.id)
        });
        Self { path, store }
    }

    /// The stable id (`1..=15`) for the client fingerprint `fp`: its remembered id, or a freshly assigned
    /// one (lowest free, else LRU-evict at the cap). Bumps the entry to MRU and persists.
    pub(crate) fn resolve(&mut self, fp: [u8; 32]) -> u32 {
        let key: String = fp.iter().map(|b| format!("{b:02x}")).collect();
        self.store.tick = self.store.tick.wrapping_add(1);
        let now = self.store.tick;

        if let Some(e) = self.store.entries.iter_mut().find(|e| e.fp == key) {
            e.seen = now;
            let id = e.id;
            self.persist();
            return id;
        }

        // New client: prefer the lowest free id in 1..=MAX_ID; if all are taken, evict the LRU entry and
        // reuse its id (the evicted client re-establishes its scaling once on its next connect).
        let id = (1..=MAX_ID)
            .find(|i| !self.store.entries.iter().any(|e| e.id == *i))
            .unwrap_or_else(|| {
                let lru = self
                    .store
                    .entries
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, e)| e.seen)
                    .map(|(i, _)| i)
                    .expect("entries are non-empty whenever every id 1..=MAX_ID is taken");
                let evicted = self.store.entries.remove(lru);
                evicted.id
            });
        self.store.entries.push(Entry {
            fp: key,
            id,
            seen: now,
        });
        self.persist();
        id
    }

    /// Persist atomically (temp file + rename). Best-effort: a write failure just means a restart may
    /// re-derive an id (one scaling re-set). Not a credential, so a plain (non-ACL'd) write is fine.
    fn persist(&self) {
        let Ok(bytes) = serde_json::to_vec_pretty(&self.store) else {
            return;
        };
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(n: u8) -> [u8; 32] {
        let mut f = [0u8; 32];
        f[0] = n;
        f
    }

    #[test]
    fn stable_across_calls_and_distinct_per_client() {
        let mut m = MonitorIdentityMap {
            path: std::env::temp_dir().join(format!("pf-id-test-{}.json", std::process::id())),
            store: Store::default(),
        };
        let a1 = m.resolve(fp(1));
        let b = m.resolve(fp(2));
        let a2 = m.resolve(fp(1));
        assert_eq!(a1, a2, "same client → same id");
        assert_ne!(a1, b, "distinct clients → distinct ids");
        assert!((1..=MAX_ID).contains(&a1) && (1..=MAX_ID).contains(&b));
        let _ = std::fs::remove_file(&m.path);
    }

    #[test]
    fn lru_eviction_reuses_an_id_at_the_cap() {
        let mut m = MonitorIdentityMap {
            path: std::env::temp_dir().join(format!("pf-id-lru-{}.json", std::process::id())),
            store: Store::default(),
        };
        // Fill all 15 ids (clients 1..=15), then touch client 2 so client 1 is the LRU.
        for n in 1..=15u8 {
            m.resolve(fp(n));
        }
        let _ = m.resolve(fp(2));
        // A 16th client evicts the LRU (client 1) and reuses its id; ids stay bounded.
        let id16 = m.resolve(fp(16));
        assert!((1..=MAX_ID).contains(&id16));
        assert_eq!(m.store.entries.len(), 15, "cap holds at 15 entries");
        assert!(m.store.entries.iter().all(|e| (1..=MAX_ID).contains(&e.id)));
        let _ = std::fs::remove_file(&m.path);
    }
}
