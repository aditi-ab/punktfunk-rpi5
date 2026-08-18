//! Where the console's settings LIVE is the host's business, not the shell's. The screens
//! mutate an in-memory [`Settings`] and ask a [`SettingsStore`] to persist it (and to hand
//! back a fresh copy before a mutation, so a whole-file writer never reverts what another
//! writer stored meanwhile — the same rebase-before-adjust discipline `screens/settings.rs`
//! has always kept). On the desktop that is the JSON file `pf_client_core::trust` owns; on
//! Android it is a snapshot round-tripped over JNI into `SharedPreferences`.
//!
//! The profile catalog rides the same seam: the console cannot create profiles (design
//! client-settings-profiles.md §5.4 — the desktop app does), it only lists and pins them,
//! so all it needs is `(id, name)` pairs from wherever the host keeps them.

use pf_client_core::trust::{KnownHosts, Settings};

/// The persistence seam between the shell and its host.
pub trait SettingsStore: Send + Sync {
    /// The settings as currently persisted. Called at construction and again immediately
    /// before every mutation (rebase), so a store that is expensive to read should cache
    /// and invalidate rather than hit disk per call.
    fn load(&self) -> Settings;
    /// Persist. Failures are the store's to log — the shell has already applied the
    /// change to its in-memory copy and shows it as done.
    fn save(&self, settings: &Settings);
    /// The profile catalog as `(id, name)`, in display order.
    fn profiles(&self) -> Vec<(String, String)>;
    /// The known-hosts store as it stands — the console builds a saved host's `punktfunk://`
    /// link from it (`DeepLink::for_host` wants the record's stable id, which a `HostRow`
    /// does not carry). Called on demand, from the "Copy link" actions only.
    fn known_hosts(&self) -> KnownHosts;
}

/// The desktop store: `pf_client_core::trust::Settings::{load, save}` and the profiles
/// file, exactly as the shell always did it. A unit struct so the shell can hold a
/// `&'static` to [`FILE_STORE`] when no host-provided store is given.
#[cfg(any(target_os = "linux", windows))]
pub struct FileSettingsStore;

#[cfg(any(target_os = "linux", windows))]
impl SettingsStore for FileSettingsStore {
    fn load(&self) -> Settings {
        Settings::load()
    }

    fn save(&self, settings: &Settings) {
        settings.save();
    }

    fn profiles(&self) -> Vec<(String, String)> {
        pf_client_core::profiles::ProfilesFile::load()
            .profiles
            .into_iter()
            .map(|p| (p.id, p.name))
            .collect()
    }

    fn known_hosts(&self) -> KnownHosts {
        KnownHosts::load()
    }
}

/// The one desktop store instance — what the Vulkan session's overlay and every test hand
/// the shell.
#[cfg(any(target_os = "linux", windows))]
pub static FILE_STORE: FileSettingsStore = FileSettingsStore;

/// The desktop store as a trait object — the shell's default when a host hands it none,
/// and what the tests' `Ctx` literals name.
#[cfg(any(target_os = "linux", windows))]
pub fn file_store() -> &'static dyn SettingsStore {
    &FILE_STORE
}

/// A store over an in-memory snapshot the host pushes into and reads back out of — the
/// Android shape, and handy for tests that must not touch a real settings file. `save`
/// replaces the snapshot and bumps a generation counter the host polls to learn there is
/// something to persist.
pub struct SnapshotStore {
    inner: std::sync::Mutex<SnapshotInner>,
}

struct SnapshotInner {
    settings: Settings,
    profiles: Vec<(String, String)>,
    /// The host's known-hosts records, as far as the console needs them (id, address,
    /// fingerprint) — pushed alongside the host rows.
    known_hosts: KnownHosts,
    /// Bumped on every `save`; the host compares against what it last persisted.
    saved_gen: u64,
}

impl SnapshotStore {
    pub fn new(settings: Settings, profiles: Vec<(String, String)>) -> SnapshotStore {
        SnapshotStore {
            inner: std::sync::Mutex::new(SnapshotInner {
                settings,
                profiles,
                known_hosts: KnownHosts::default(),
                saved_gen: 0,
            }),
        }
    }

    /// Host side: replace what the shell will read next (a settings change made elsewhere —
    /// the touch UI, a deep link). Does not count as a save.
    pub fn set(&self, settings: Settings) {
        self.inner.lock().unwrap().settings = settings;
    }

    pub fn set_profiles(&self, profiles: Vec<(String, String)>) {
        self.inner.lock().unwrap().profiles = profiles;
    }

    /// Host side: the known-hosts records the console may build links from.
    pub fn set_known_hosts(&self, hosts: KnownHosts) {
        self.inner.lock().unwrap().known_hosts = hosts;
    }

    /// Host side: the save generation alone — cheap enough to compare every frame.
    pub fn saved_gen(&self) -> u64 {
        self.inner.lock().unwrap().saved_gen
    }

    /// Host side: the current snapshot and the save generation — persist when the
    /// generation moved since the last look.
    pub fn snapshot(&self) -> (Settings, u64) {
        let g = self.inner.lock().unwrap();
        (g.settings.clone(), g.saved_gen)
    }
}

impl SettingsStore for SnapshotStore {
    fn load(&self) -> Settings {
        self.inner.lock().unwrap().settings.clone()
    }

    fn save(&self, settings: &Settings) {
        let mut g = self.inner.lock().unwrap();
        g.settings = settings.clone();
        g.saved_gen += 1;
    }

    fn profiles(&self) -> Vec<(String, String)> {
        self.inner.lock().unwrap().profiles.clone()
    }

    fn known_hosts(&self) -> KnownHosts {
        let g = self.inner.lock().unwrap();
        KnownHosts {
            hosts: g.known_hosts.hosts.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_store_round_trips_and_counts_saves() {
        let store = SnapshotStore::new(Settings::default(), vec![("p1".into(), "Work".into())]);
        assert_eq!(store.snapshot().1, 0);
        let mut s = store.load();
        s.ui_palette = "mint".into();
        store.save(&s);
        let (after, generation) = store.snapshot();
        assert_eq!(after.ui_palette, "mint");
        assert_eq!(generation, 1);
        assert_eq!(
            store.profiles(),
            vec![("p1".to_string(), "Work".to_string())]
        );
        // A host push replaces the snapshot without counting as a save.
        store.set(Settings::default());
        assert_eq!(store.load().ui_palette, Settings::default().ui_palette);
        assert_eq!(store.snapshot().1, 1);
    }
}
