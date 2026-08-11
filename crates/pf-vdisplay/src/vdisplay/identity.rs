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
//! * **Mutter** can't carry the id into its virtual monitor (fresh EDID serial per `RecordVirtual`,
//!   no API to override), so GNOME's `monitors.xml` never rematches — the host persists the scale
//!   itself instead ([`ScaleMap`], keyed by the same [`identity_key`]) and the Mutter backend
//!   reapplies it. The slot id still keys the registry's group arrangement/state.
//!
//! The map key is a composable string ([`identity_key`]): the client cert fingerprint alone
//! (`per-client`), or fingerprint + resolution (`per-client-mode` — distinct scaling per resolution).
//! Anonymous/TOFU/GameStream sessions have no fingerprint and resolve to id `0` (auto) upstream,
//! never reaching this map.
//!
//! Persisted to `<config>/display-identity.json` (migrated from the legacy Windows
//! `pf-vdisplay-identity.json`) so ids — and the client→config association — survive host restarts.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

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
        let dir = pf_paths::config_dir();
        let path = dir.join(FILE);
        let (from, bytes) = match std::fs::read(&path) {
            Ok(b) => (path.clone(), Some(b)),
            Err(_) => {
                let legacy = dir.join(LEGACY_FILE);
                match std::fs::read(&legacy) {
                    Ok(b) => (legacy, Some(b)),
                    // No file at all is the ordinary first-run case — not worth a word.
                    Err(_) => (path.clone(), None),
                }
            }
        };
        let mut store = match bytes {
            Some(b) => match serde_json::from_slice::<Store>(&b) {
                Ok(s) => s,
                Err(e) => {
                    // An UNPARSEABLE map used to be swallowed into `Default::default()`, and the very
                    // next `resolve` persisted that empty store OVER the file — silently discarding
                    // every client's Windows EDID serial / KWin `Virtual-punktfunk-<id>` and the
                    // per-display DPI the OS keyed to them. Say so, and move the file aside so the
                    // damage is recoverable by hand (same treatment `display-presets.json` gets).
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
    ///
    /// `live` is the set of ids that currently drive a REAL display (the Windows manager's slot keys
    /// / the Linux pool's `identity_slot`s). An id in it is never evicted, and when every eviction
    /// candidate is live this **refuses** (`None`) rather than handing the newcomer an id that is
    /// already someone else's monitor. That is not hypothetical: the id keys the Windows manager's
    /// slot map, whose plain-JOIN branch attaches an arriving session to whatever monitor the slot
    /// already holds — so evicting a live id handed client B client A's streaming monitor, capture
    /// target and all. Refusing costs the newcomer its stable identity (upstream falls back to the
    /// shared/auto slot: `resolve_slot` → `None`, `slot_id_for` → `0`); evicting cost a live client
    /// its session.
    pub(crate) fn resolve(&mut self, key: &str, live: &BTreeSet<u32>) -> Option<u32> {
        self.store.tick = self.store.tick.wrapping_add(1);
        let now = self.store.tick;

        if let Some(e) = self.store.entries.iter_mut().find(|e| e.key == key) {
            e.seen = now;
            let id = e.id;
            self.persist();
            return Some(id);
        }

        // New client: prefer the lowest free id in 1..=MAX_ID; if all are taken, evict the
        // least-recently-seen entry that is NOT live and reuse its id (that client re-establishes its
        // scaling once on its next connect).
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

    /// Persist atomically (temp file + rename). Best-effort: a write failure just means a restart may
    /// re-derive an id (one scaling re-set). The CONTENTS are not a credential, so the file itself
    /// is a plain write — but the directory it lands in is `pf_paths::config_dir()`, which is, so it
    /// is created with the 0700 helper rather than `create_dir_all`.
    fn persist(&self) {
        let Ok(bytes) = serde_json::to_vec_pretty(&self.store) else {
            return;
        };
        if let Some(dir) = self.path.parent() {
            // `create_private_dir`, not `create_dir_all`: this is `pf_paths::config_dir()`, which
            // also holds the host private key, the pairing allow-list and the mgmt token. Every
            // other writer in the workspace uses the 0700 helper; these two did not.
            let _ = pf_paths::create_private_dir(dir);
        }
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

/// The process-wide identity map (persisted, loaded once). Shared by the Windows manager and the
/// Linux KWin/Mutter backends — never in the same process (a host runs one platform), so one
/// instance ⇒ no clobbering of the shared `display-identity.json`.
pub(crate) fn global() -> &'static Mutex<DisplayIdentityMap> {
    static MAP: OnceLock<Mutex<DisplayIdentityMap>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(DisplayIdentityMap::load()))
}

/// Resolve the connecting client's stable slot id per the `identity` policy. When no policy is
/// configured, `default` applies — **PerClient on Windows / Shared on Linux**, preserving each
/// platform's historical behavior (Windows always keyed monitors per-client; Linux used one shared
/// output name). `None` ⇒ shared / anonymous (or the map [refused](DisplayIdentityMap::resolve) an
/// id because every one is live) → the backend uses its base name / auto slot.
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
    // Sample the live ids BEFORE taking the map lock, never under it: the sources below take the
    // Windows manager's `state` lock / the Linux pool lock, and this map is reached from inside a
    // backend `create` — a lock order of (display owner → identity map) in both directions would be
    // a deadlock. One direction only, and the map lock stays a leaf.
    let live = live_slot_ids();
    global()
        .lock()
        .unwrap()
        .resolve(&identity_key(fp, mode, per_client_mode), &live)
}

/// The identity slots currently driving a REAL display — the eviction guard for
/// [`DisplayIdentityMap::resolve`]. Windows reads the manager's slot map (the key IS the identity
/// slot); Linux reads the registry pool's per-entry `identity_slot`. Both include KEPT
/// (lingering/pinned) displays on purpose: a kept display is a live compositor/driver resource whose
/// owner is expected back, and the whole point of the id is that the reconnect finds it again.
/// Anonymous (`0`) is not an identity and never blocks an assignment.
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

// ---------------------------------------------------------------------------------------
// Per-client persisted desktop scale (`<config>/display-scale.json`) — the Mutter carrier
// ---------------------------------------------------------------------------------------

/// The scale-map filename.
const SCALE_FILE: &str = "display-scale.json";

/// The key under which a client's desktop **scale** is persisted: the [`identity_key`] under a
/// per-client policy, or the fixed `"shared"` slot under a Shared policy / for anonymous sessions
/// (can't collide — identity keys are 64 hex chars). Same policy resolution as [`resolve_slot`].
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

/// Pure core of [`scale_key`] (policy already resolved) — unit-testable without the global store.
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

/// Persistent client-key → desktop-scale map. Windows and KDE persist per-display scaling
/// themselves once the backend gives the monitor a stable identity — but GNOME **cannot**: Mutter
/// mints a fresh EDID serial (`0x%.6x`, a per-shell counter) for every `RecordVirtual` monitor, so
/// the `monitors.xml` entry GNOME writes never rematches on reconnect, and `RecordVirtual` offers
/// no way to pass a stable identity. The host therefore remembers the scale itself; the Mutter
/// backend reapplies it (`preferred-scale` + its topology `ApplyMonitorsConfig`) and records the
/// user's in-session changes here.
pub(crate) struct ScaleMap {
    path: PathBuf,
    map: std::collections::BTreeMap<String, f64>,
}

impl ScaleMap {
    /// Load the persisted map (empty on first run / unreadable file — a client just re-sets its
    /// scaling once). Drops non-finite / out-of-range entries from a hand-edited file.
    fn load() -> Self {
        let path = pf_paths::config_dir().join(SCALE_FILE);
        let mut map: std::collections::BTreeMap<String, f64> = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        map.retain(|_, s| s.is_finite() && (0.25..=8.0).contains(s));
        Self { path, map }
    }

    /// The remembered scale for `key`, if any.
    pub(crate) fn get(&self, key: &str) -> Option<f64> {
        self.map.get(key).copied()
    }

    /// Remember `scale` for `key` and persist (atomic temp-write + rename, best-effort — a failed
    /// write costs one scaling re-set after a restart).
    pub(crate) fn set(&mut self, key: &str, scale: f64) {
        if !scale.is_finite() || !(0.25..=8.0).contains(&scale) {
            return;
        }
        self.map.insert(key.to_string(), scale);
        let Ok(bytes) = serde_json::to_vec_pretty(&self.map) else {
            return;
        };
        if let Some(dir) = self.path.parent() {
            // `create_private_dir`, not `create_dir_all`: this is `pf_paths::config_dir()`, which
            // also holds the host private key, the pairing allow-list and the mgmt token. Every
            // other writer in the workspace uses the 0700 helper; these two did not.
            let _ = pf_paths::create_private_dir(dir);
        }
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

/// The process-wide scale map (persisted, loaded once) — used by the Mutter backend only (see
/// [`ScaleMap`]).
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

    /// Nothing is streaming — the ordinary case, where the live set never constrains anything.
    fn nothing_live() -> BTreeSet<u32> {
        BTreeSet::new()
    }

    #[test]
    fn stable_across_calls_and_distinct_per_client() {
        let mut m = temp_map("stable");
        let a1 = m.resolve(&identity_key(fp(1), (1920, 1080), false), &nothing_live());
        let b = m.resolve(&identity_key(fp(2), (1920, 1080), false), &nothing_live());
        // per-client: mode ignored
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
        // touch 2 so 1 is LRU
        let _ = m.resolve(&identity_key(fp(2), (1920, 1080), false), &nothing_live());
        let id16 = m
            .resolve(&identity_key(fp(16), (1920, 1080), false), &nothing_live())
            .expect("nothing is live → the LRU id is free to take");
        assert!((1..=MAX_ID).contains(&id16));
        assert_eq!(m.store.entries.len(), 15, "cap holds at 15 entries");
        assert!(m.store.entries.iter().all(|e| (1..=MAX_ID).contains(&e.id)));
        let _ = std::fs::remove_file(&m.path);
    }

    /// 10.2: the LRU victim is chosen among ids that are NOT driving a display. Handing the LRU id
    /// to a newcomer while its owner streams is what let the Windows manager's plain-JOIN branch
    /// attach the newcomer to the live client's monitor.
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
        // fp(1) is the least-recently-seen — and it is the one that is streaming.
        let lru_id = ids[0];
        let live: BTreeSet<u32> = [lru_id].into_iter().collect();
        let id16 = m
            .resolve(&identity_key(fp(16), (1920, 1080), false), &live)
            .expect("14 idle ids remain — one of them is the victim");
        assert_ne!(id16, lru_id, "must not take the id of a live display");
        assert_eq!(id16, ids[1], "the next-least-recently-seen IDLE id instead");
        // The live client's mapping is untouched, so its reconnect still finds its own display.
        assert_eq!(
            m.resolve(&identity_key(fp(1), (1920, 1080), false), &live),
            Some(lru_id)
        );
        let _ = std::fs::remove_file(&m.path);
    }

    /// Fail-closed at the extreme: every id live ⇒ refuse, rather than displace a streaming client.
    /// The caller degrades to the shared/auto identity (`resolve_slot` → `None`, `slot_id_for` → 0).
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
        // A KNOWN client is still resolved even when everything is live — it owns that id already.
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
        // Shared / anonymous → the fixed shared slot.
        assert_eq!(
            scale_key_for(Identity::Shared, Some(fp(1)), (1920, 1080)),
            "shared"
        );
        assert_eq!(
            scale_key_for(Identity::PerClient, None, (1920, 1080)),
            "shared"
        );
        // Per-client → the fingerprint key; per-client-mode appends the resolution.
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
        m.set("bad-nan", f64::NAN); // ignored
        m.set("bad-range", 100.0); // ignored
        assert_eq!(m.get("k"), Some(1.5));
        assert_eq!(m.get("bad-nan"), None);
        assert_eq!(m.get("bad-range"), None);
        // Persisted: a fresh map from the same path sees the value.
        let bytes = std::fs::read(&path).unwrap();
        let reread: std::collections::BTreeMap<String, f64> =
            serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reread.get("k"), Some(&1.5));
        let _ = std::fs::remove_file(&path);
    }
}
