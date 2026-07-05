//! Platform-neutral **per-client → stable display-id map** (design: `design/display-management.md`
//! §5.4 — identity). A client that reconnects gets the SAME small stable id every time, so the
//! desktop environment can key its per-display config (notably **DPI scaling**) to it and reapply it:
//!
//! * **Windows** seeds the pf-vdisplay monitor's EDID serial + IddCx `ConnectorIndex` from the id, so
//!   Windows reapplies the client's saved `PerMonitorSettings` scaling. The id must stay `1..=15`
//!   (`ConnectorIndex < MaxMonitorsSupported = 16`).
//! * **KWin** names the streamed output `Virtual-punktfunk-<id>`; KWin persists per-output scale/mode
//!   in `kwinoutputconfig.json` matched by name, so a stable per-client name makes KDE reapply that
//!   client's scaling. (Generalised here from the Windows-only map; the KWin wiring is Stage 3.)
//!
//! The map key is a composable string ([`identity_key`]): the client cert fingerprint alone
//! (`per-client`), or fingerprint + resolution (`per-client-mode` — distinct scaling per resolution).
//! Anonymous/TOFU/GameStream sessions have no fingerprint and resolve to id `0` (auto) upstream,
//! never reaching this map.
//!
//! Persisted to `<config>/display-identity.json` (migrated from the legacy Windows
//! `pf-vdisplay-identity.json`) so ids — and the client→config association — survive host restarts.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Max stable id. Bounded by the Windows driver's use of the id as the IddCx `ConnectorIndex`
/// (`< MaxMonitorsSupported = 16`), so ids run `1..=15` on every platform for a single shared map.
const MAX_ID: u32 = 15;

/// The map filename (migrated from the legacy Windows-only `pf-vdisplay-identity.json`).
const FILE: &str = "display-identity.json";
const LEGACY_FILE: &str = "pf-vdisplay-identity.json";

/// Compose the map key for a client. `per_client_mode` appends the resolution so a client keeps a
/// distinct id (and thus distinct persisted scaling) per resolution; otherwise the fingerprint alone.
pub(crate) fn identity_key(fp: [u8; 32], mode: (u32, u32), per_client_mode: bool) -> String {
    let hex: String = fp.iter().map(|b| format!("{b:02x}")).collect();
    if per_client_mode {
        format!("{hex}@{}x{}", mode.0, mode.1)
    } else {
        hex
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Store {
    /// Monotonic most-recently-used counter (the entry with the highest `seen` is the MRU). Persisted so
    /// the LRU ordering survives host restarts.
    tick: u64,
    entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    /// The composed client key ([`identity_key`]) — the map key. (Serialized as `fp` for
    /// back-compat with the legacy Windows `pf-vdisplay-identity.json`.)
    #[serde(rename = "fp")]
    key: String,
    /// The client's stable display id (`1..=15`).
    id: u32,
    /// MRU stamp (compared against [`Store::tick`]).
    seen: u64,
}

/// Persistent client-key → stable-id map (see the module docs).
pub(crate) struct DisplayIdentityMap {
    path: PathBuf,
    store: Store,
}

impl DisplayIdentityMap {
    /// Load the persisted map (empty on first run / unreadable / parse failure — a fresh map just
    /// re-derives ids, costing a client one scaling re-set the first time). Migrates the legacy
    /// Windows `pf-vdisplay-identity.json` if the new file is absent.
    pub(crate) fn load() -> Self {
        let dir = crate::gamestream::config_dir();
        let path = dir.join(FILE);
        let bytes = std::fs::read(&path)
            .or_else(|_| std::fs::read(dir.join(LEGACY_FILE)))
            .ok();
        let mut store = bytes
            .and_then(|b| serde_json::from_slice::<Store>(&b).ok())
            .unwrap_or_default();
        // SANITIZE a hand-edited / corrupt / cross-version file before trusting it: resolve()'s
        // found-entry branch returns the stored id verbatim, so an out-of-range id (0 = the "auto"
        // sentinel, or > MAX_ID) or a duplicate id/key would flow straight into the display identity.
        // Drop out-of-range ids and dedup by BOTH key and id (keeping the most-recently-seen on a
        // clash) so no two clients can map to the same id.
        store.entries.sort_by_key(|e| std::cmp::Reverse(e.seen));
        let mut seen_key = std::collections::HashSet::new();
        let mut seen_id = std::collections::HashSet::new();
        store.entries.retain(|e| {
            (1..=MAX_ID).contains(&e.id) && seen_key.insert(e.key.clone()) && seen_id.insert(e.id)
        });
        Self { path, store }
    }

    /// The stable id (`1..=15`) for the client `key` ([`identity_key`]): its remembered id, or a
    /// freshly assigned one (lowest free, else LRU-evict at the cap). Bumps the entry to MRU and persists.
    pub(crate) fn resolve(&mut self, key: &str) -> u32 {
        self.store.tick = self.store.tick.wrapping_add(1);
        let now = self.store.tick;

        if let Some(e) = self.store.entries.iter_mut().find(|e| e.key == key) {
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
            key: key.to_string(),
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

    fn temp_map(tag: &str) -> DisplayIdentityMap {
        DisplayIdentityMap {
            path: std::env::temp_dir().join(format!("pf-id-{tag}-{}.json", std::process::id())),
            store: Store::default(),
        }
    }

    #[test]
    fn stable_across_calls_and_distinct_per_client() {
        let mut m = temp_map("stable");
        let a1 = m.resolve(&identity_key(fp(1), (1920, 1080), false));
        let b = m.resolve(&identity_key(fp(2), (1920, 1080), false));
        let a2 = m.resolve(&identity_key(fp(1), (1280, 720), false)); // per-client: mode ignored
        assert_eq!(a1, a2, "same client → same id (per-client ignores mode)");
        assert_ne!(a1, b, "distinct clients → distinct ids");
        assert!((1..=MAX_ID).contains(&a1) && (1..=MAX_ID).contains(&b));
        let _ = std::fs::remove_file(&m.path);
    }

    #[test]
    fn per_client_mode_splits_by_resolution() {
        let mut m = temp_map("permode");
        let hd = m.resolve(&identity_key(fp(1), (1920, 1080), true));
        let uhd = m.resolve(&identity_key(fp(1), (3840, 2160), true));
        let hd2 = m.resolve(&identity_key(fp(1), (1920, 1080), true));
        assert_ne!(hd, uhd, "same client, different resolution → different id");
        assert_eq!(hd, hd2, "same client + resolution → same id");
        let _ = std::fs::remove_file(&m.path);
    }

    #[test]
    fn lru_eviction_reuses_an_id_at_the_cap() {
        let mut m = temp_map("lru");
        for n in 1..=15u8 {
            m.resolve(&identity_key(fp(n), (1920, 1080), false));
        }
        let _ = m.resolve(&identity_key(fp(2), (1920, 1080), false)); // touch 2 so 1 is LRU
        let id16 = m.resolve(&identity_key(fp(16), (1920, 1080), false));
        assert!((1..=MAX_ID).contains(&id16));
        assert_eq!(m.store.entries.len(), 15, "cap holds at 15 entries");
        assert!(m.store.entries.iter().all(|e| (1..=MAX_ID).contains(&e.id)));
        let _ = std::fs::remove_file(&m.path);
    }

    #[test]
    fn key_composition() {
        assert_eq!(identity_key(fp(0xab), (1920, 1080), false).len(), 64); // hex fp only
        assert!(identity_key(fp(0xab), (1920, 1080), true).ends_with("@1920x1080"));
    }
}
