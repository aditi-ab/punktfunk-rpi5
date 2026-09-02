//! Per-client → stable display-id map. A reconnect reuses the same small id so
//! the compositor can reapply per-display scale. Design: `design/display-management.md`.
//!
//! Ids stay `1..=15` on every platform (Windows IddCx `ConnectorIndex` is
//! `< MaxMonitorsSupported` = 16). The key is cert fingerprint, or fingerprint
//! plus resolution (`per-client-mode`). Sessions with no fingerprint never
//! reach this map (id `0` / auto, upstream).
//!
//! Persist path: `<config>/display-identity.json` (migrates
//! `pf-vdisplay-identity.json`). GNOME cannot rematch a virtual monitor, so
//! [`ScaleMap`] stores scale under the same [`identity_key`].

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// IddCx `ConnectorIndex` is `< MaxMonitorsSupported` (16). Shared map: `1..=15`.
const MAX_ID: u32 = 15;

const FILE: &str = "display-identity.json";
const LEGACY_FILE: &str = "pf-vdisplay-identity.json";

/// Fingerprint hex; `{hex}@{w}x{h}` when `per_client_mode` so each resolution keeps its scale.
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
    /// MRU counter; persisted so LRU order survives a host restart.
    tick: u64,
    entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    /// Serialized as `fp` for back-compat with `pf-vdisplay-identity.json`.
    #[serde(rename = "fp")]
    key: String,
    id: u32,
    seen: u64,
}

pub(crate) struct DisplayIdentityMap {
    path: PathBuf,
    store: Store,
}

impl DisplayIdentityMap {
    /// Empty on first run or parse failure (ids re-derive once). Falls back to `pf-vdisplay-identity.json`.
    pub(crate) fn load() -> Self {
        let dir = pf_paths::config_dir();
        let path = dir.join(FILE);
        let (from, bytes) = match std::fs::read(&path) {
            Ok(b) => (path.clone(), Some(b)),
            Err(_) => {
                let legacy = dir.join(LEGACY_FILE);
                match std::fs::read(&legacy) {
                    Ok(b) => (legacy, Some(b)),
                    Err(_) => (path.clone(), None),
                }
            }
        };
        let mut store = match bytes {
            Some(b) => match serde_json::from_slice::<Store>(&b) {
                Ok(s) => s,
                Err(e) => {
                    // Rename aside so the next persist cannot overwrite an unreadable map
                    // (same as `display-presets.json`). Recover by hand from `.json.bad`.
                    tracing::warn!(
                        path = %from.display(),
                        error = %e,
                        "display-identity map is unreadable — starting a fresh one; \
                         the old file is kept as .bad (every client re-derives its display id once)"
                    );
                    let _ = std::fs::rename(&from, from.with_extension("json.bad"));
                    Store::default()
                }
            },
            None => Store::default(),
        };
        // `resolve` returns a stored id as-is. Drop 0 / >MAX_ID and duplicate key or id
        // (keep MRU) so a hand-edited file cannot collide two clients onto one slot.
        store.entries.sort_by_key(|e| std::cmp::Reverse(e.seen));
        let mut seen_key = std::collections::HashSet::new();
        let mut seen_id = std::collections::HashSet::new();
        store.entries.retain(|e| {
            (1..=MAX_ID).contains(&e.id) && seen_key.insert(e.key.clone()) && seen_id.insert(e.id)
        });
        Self { path, store }
    }

    /// Remembered id for `key`, or the lowest free / LRU-idle id. Bumps MRU and persists.
    ///
    /// `live` ids currently drive a real display. Never evict those: the Windows
    /// slot map JOIN-attaches a newcomer to whatever monitor that slot already holds.
    /// If every candidate is live, return `None` (caller falls back to auto slot 0).
    pub(crate) fn resolve(&mut self, key: &str, live: &BTreeSet<u32>) -> Option<u32> {
        self.store.tick = self.store.tick.wrapping_add(1);
        let now = self.store.tick;

        if let Some(e) = self.store.entries.iter_mut().find(|e| e.key == key) {
            e.seen = now;
            let id = e.id;
            self.persist();
            return Some(id);
        }

        let id = match (1..=MAX_ID).find(|i| !self.store.entries.iter().any(|e| e.id == *i)) {
            Some(free) => free,
            None => {
                let lru = self
                    .store
                    .entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| !live.contains(&e.id))
                    .min_by_key(|(_, e)| e.seen)
                    .map(|(i, _)| i);
                let Some(lru) = lru else {
                    tracing::warn!(
                        cap = MAX_ID,
                        live = live.len(),
                        "display identity map is full and every id is driving a live display — \
                         this client gets the shared/auto display identity (no persisted per-client \
                         scaling) rather than displacing a live one"
                    );
                    return None;
                };
                self.store.entries.remove(lru).id
            }
        };
        self.store.entries.push(Entry {
            key: key.to_string(),
            id,
            seen: now,
        });
        self.persist();
        Some(id)
    }

    /// Temp-file + rename. Best-effort. Parent is `config_dir()` (host key, allow-list,
    /// mgmt token) so use `create_private_dir` (0700), not `create_dir_all`.
    fn persist(&self) {
        let Ok(bytes) = serde_json::to_vec_pretty(&self.store) else {
            return;
        };
        if let Some(dir) = self.path.parent() {
            let _ = pf_paths::create_private_dir(dir);
        }
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

/// Process-wide map, loaded once. One host process per platform, so one
/// instance cannot clobber `display-identity.json` from a second backend.
pub(crate) fn global() -> &'static Mutex<DisplayIdentityMap> {
    static MAP: OnceLock<Mutex<DisplayIdentityMap>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(DisplayIdentityMap::load()))
}

/// Slot for this client under the identity policy, or `default` if none is set
/// (PerClient on Windows, Shared on Linux). `None` is shared/anonymous, or the
/// map refused because every id is live — backend uses its base name / auto slot.
pub(crate) fn resolve_slot(
    fp: Option<[u8; 32]>,
    mode: (u32, u32),
    default: crate::policy::Identity,
) -> Option<u32> {
    use crate::policy::Identity;
    let id_policy = crate::policy::prefs()
        .configured_effective()
        .map(|e| e.identity)
        .unwrap_or(default);
    let per_client_mode = match id_policy {
        Identity::Shared => return None,
        Identity::PerClient => false,
        Identity::PerClientMode => true,
    };
    let fp = fp?;
    // Sample live ids before the map lock. Their sources take the manager/pool
    // lock, and this map is reached from backend `create`. Map lock stays a leaf.
    let live = live_slot_ids();
    global()
        .lock()
        .unwrap()
        .resolve(&identity_key(fp, mode, per_client_mode), &live)
}

/// Ids driving a real display, including KEPT (lingering) ones: the reconnect
/// must find that slot again. `0` is not an identity and does not block assignment.
fn live_slot_ids() -> BTreeSet<u32> {
    #[cfg(target_os = "windows")]
    {
        crate::manager::snapshot()
            .into_iter()
            .map(|i| i.slot_id)
            .filter(|s| *s != 0)
            .collect()
    }
    #[cfg(target_os = "linux")]
    {
        crate::registry::live_identity_slots()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        BTreeSet::new()
    }
}

const SCALE_FILE: &str = "display-scale.json";

/// Scale-map key: [`identity_key`], or `"shared"` for Shared/anonymous.
/// `"shared"` cannot collide — identity keys are 64 hex chars.
pub(crate) fn scale_key(
    fp: Option<[u8; 32]>,
    mode: (u32, u32),
    default: crate::policy::Identity,
) -> String {
    let id_policy = crate::policy::prefs()
        .configured_effective()
        .map(|e| e.identity)
        .unwrap_or(default);
    scale_key_for(id_policy, fp, mode)
}

/// [`scale_key`] with policy already resolved (no global prefs).
fn scale_key_for(
    policy: crate::policy::Identity,
    fp: Option<[u8; 32]>,
    mode: (u32, u32),
) -> String {
    use crate::policy::Identity;
    match (policy, fp) {
        (Identity::Shared, _) | (_, None) => "shared".to_string(),
        (Identity::PerClient, Some(fp)) => identity_key(fp, mode, false),
        (Identity::PerClientMode, Some(fp)) => identity_key(fp, mode, true),
    }
}

/// Client-key → desktop-scale. GNOME never rematches `RecordVirtual` EDIDs
/// (fresh serial, no override), so the host stores scale and the Mutter backend
/// reapplies it. Windows/KDE persist scale themselves once the id is stable.
pub(crate) struct ScaleMap {
    path: PathBuf,
    map: std::collections::BTreeMap<String, f64>,
}

impl ScaleMap {
    /// Empty on first run / unreadable. Drop non-finite and values outside 0.25..=8.0
    /// (sane compositor scale range; a hand-edited file can store anything).
    fn load() -> Self {
        let path = pf_paths::config_dir().join(SCALE_FILE);
        let mut map: std::collections::BTreeMap<String, f64> = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        map.retain(|_, s| s.is_finite() && (0.25..=8.0).contains(s));
        Self { path, map }
    }

    pub(crate) fn get(&self, key: &str) -> Option<f64> {
        self.map.get(key).copied()
    }

    pub(crate) fn set(&mut self, key: &str, scale: f64) {
        if !scale.is_finite() || !(0.25..=8.0).contains(&scale) {
            return;
        }
        self.map.insert(key.to_string(), scale);
        let Ok(bytes) = serde_json::to_vec_pretty(&self.map) else {
            return;
        };
        if let Some(dir) = self.path.parent() {
            // Parent is `config_dir()` (host key, allow-list, token). 0700, not `create_dir_all`.
            let _ = pf_paths::create_private_dir(dir);
        }
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

/// Process-wide scale map, loaded once. Mutter backend only.
pub(crate) fn scales() -> &'static Mutex<ScaleMap> {
    static MAP: OnceLock<Mutex<ScaleMap>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(ScaleMap::load()))
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

    fn nothing_live() -> BTreeSet<u32> {
        BTreeSet::new()
    }

    #[test]
    fn stable_across_calls_and_distinct_per_client() {
        let mut m = temp_map("stable");
        let a1 = m.resolve(&identity_key(fp(1), (1920, 1080), false), &nothing_live());
        let b = m.resolve(&identity_key(fp(2), (1920, 1080), false), &nothing_live());
        let a2 = m.resolve(&identity_key(fp(1), (1280, 720), false), &nothing_live());
        assert_eq!(a1, a2, "same client → same id (per-client ignores mode)");
        assert_ne!(a1, b, "distinct clients → distinct ids");
        assert!(a1.is_some_and(|i| (1..=MAX_ID).contains(&i)));
        assert!(b.is_some_and(|i| (1..=MAX_ID).contains(&i)));
        let _ = std::fs::remove_file(&m.path);
    }

    #[test]
    fn per_client_mode_splits_by_resolution() {
        let mut m = temp_map("permode");
        let hd = m.resolve(&identity_key(fp(1), (1920, 1080), true), &nothing_live());
        let uhd = m.resolve(&identity_key(fp(1), (3840, 2160), true), &nothing_live());
        let hd2 = m.resolve(&identity_key(fp(1), (1920, 1080), true), &nothing_live());
        assert_ne!(hd, uhd, "same client, different resolution → different id");
        assert_eq!(hd, hd2, "same client + resolution → same id");
        let _ = std::fs::remove_file(&m.path);
    }

    #[test]
    fn lru_eviction_reuses_an_id_at_the_cap() {
        let mut m = temp_map("lru");
        for n in 1..=15u8 {
            m.resolve(&identity_key(fp(n), (1920, 1080), false), &nothing_live());
        }
        // Touch 2 so 1 is LRU.
        let _ = m.resolve(&identity_key(fp(2), (1920, 1080), false), &nothing_live());
        let id16 = m
            .resolve(&identity_key(fp(16), (1920, 1080), false), &nothing_live())
            .expect("nothing is live → the LRU id is free to take");
        assert!((1..=MAX_ID).contains(&id16));
        assert_eq!(m.store.entries.len(), 15, "cap holds at 15 entries");
        assert!(m.store.entries.iter().all(|e| (1..=MAX_ID).contains(&e.id)));
        let _ = std::fs::remove_file(&m.path);
    }

    #[test]
    fn lru_eviction_never_takes_a_live_id() {
        let mut m = temp_map("lru-live");
        let mut ids = Vec::new();
        for n in 1..=15u8 {
            ids.push(
                m.resolve(&identity_key(fp(n), (1920, 1080), false), &nothing_live())
                    .unwrap(),
            );
        }
        // fp(1) is LRU and live.
        let lru_id = ids[0];
        let live: BTreeSet<u32> = [lru_id].into_iter().collect();
        let id16 = m
            .resolve(&identity_key(fp(16), (1920, 1080), false), &live)
            .expect("14 idle ids remain — one of them is the victim");
        assert_ne!(id16, lru_id, "must not take the id of a live display");
        assert_eq!(id16, ids[1], "the next-least-recently-seen IDLE id instead");
        assert_eq!(
            m.resolve(&identity_key(fp(1), (1920, 1080), false), &live),
            Some(lru_id)
        );
        let _ = std::fs::remove_file(&m.path);
    }

    #[test]
    fn refuses_rather_than_evicting_when_every_id_is_live() {
        let mut m = temp_map("lru-all-live");
        let mut live = BTreeSet::new();
        for n in 1..=15u8 {
            live.insert(
                m.resolve(&identity_key(fp(n), (1920, 1080), false), &BTreeSet::new())
                    .unwrap(),
            );
        }
        assert_eq!(
            m.resolve(&identity_key(fp(16), (1920, 1080), false), &live),
            None
        );
        assert_eq!(m.store.entries.len(), 15, "nothing was evicted");
        // Known client still resolves: it already owns that id.
        assert!(m
            .resolve(&identity_key(fp(3), (1920, 1080), false), &live)
            .is_some());
        let _ = std::fs::remove_file(&m.path);
    }

    #[test]
    fn key_composition() {
        assert_eq!(identity_key(fp(0xab), (1920, 1080), false).len(), 64); // hex fp only
        assert!(identity_key(fp(0xab), (1920, 1080), true).ends_with("@1920x1080"));
    }

    #[test]
    fn scale_key_follows_the_identity_policy() {
        use crate::policy::Identity;
        assert_eq!(
            scale_key_for(Identity::Shared, Some(fp(1)), (1920, 1080)),
            "shared"
        );
        assert_eq!(
            scale_key_for(Identity::PerClient, None, (1920, 1080)),
            "shared"
        );
        let pc = scale_key_for(Identity::PerClient, Some(fp(1)), (1920, 1080));
        assert_eq!(pc, identity_key(fp(1), (1920, 1080), false));
        let pcm = scale_key_for(Identity::PerClientMode, Some(fp(1)), (1920, 1080));
        assert!(pcm.ends_with("@1920x1080"));
    }

    #[test]
    fn scale_map_roundtrips_and_rejects_junk() {
        let path = std::env::temp_dir().join(format!("pf-scale-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut m = ScaleMap {
            path: path.clone(),
            map: Default::default(),
        };
        assert_eq!(m.get("k"), None);
        m.set("k", 1.5);
        m.set("bad-nan", f64::NAN);
        m.set("bad-range", 100.0);
        assert_eq!(m.get("k"), Some(1.5));
        assert_eq!(m.get("bad-nan"), None);
        assert_eq!(m.get("bad-range"), None);
        let bytes = std::fs::read(&path).unwrap();
        let reread: std::collections::BTreeMap<String, f64> =
            serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reread.get("k"), Some(&1.5));
        let _ = std::fs::remove_file(&path);
    }
}
