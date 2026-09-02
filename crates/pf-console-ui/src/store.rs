//! Persistence seam between the console shell and its host.
//!
//! Screens mutate in-memory [`Settings`] and persist through [`SettingsStore`].
//! Call `load` immediately before every write: a whole-file store otherwise
//! reverts another writer's save. Desktop is `FileSettingsStore`
//! (`pf_client_core::trust` JSON). Android is [`SnapshotStore`] — the host
//! pushes a snapshot in and polls `saved_gen` out.
//!
//! Profiles are `(id, name)` in display order. The console lists and pins;
//! it does not create. Design: `design/client-settings-profiles.md`.

use pf_client_core::trust::{KnownHosts, Settings};

pub trait SettingsStore: Send + Sync {
    /// Called at construction and immediately before every mutation. Cache rather
    /// than hit disk per call.
    fn load(&self) -> Settings;
    /// Failures are the store's to log. The shell has already applied the change
    /// in memory and shows it as done.
    fn save(&self, settings: &Settings);
    fn profiles(&self) -> Vec<(String, String)>;
    /// Copy-link only. `DeepLink::for_host` needs the record's stable id, which
    /// a `HostRow` does not carry.
    fn known_hosts(&self) -> KnownHosts;
}

/// Desktop JSON via `pf_client_core::trust`. Unit struct so the shell can hold
/// a `&'static` to [`FILE_STORE`] when the host provides none.
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

#[cfg(any(target_os = "linux", windows))]
pub static FILE_STORE: FileSettingsStore = FileSettingsStore;

/// Default store when the host provides none.
#[cfg(any(target_os = "linux", windows))]
pub fn file_store() -> &'static dyn SettingsStore {
    &FILE_STORE
}

/// In-memory snapshot the host pushes and polls. `save` replaces it and bumps
/// `saved_gen`; `set` does not. Android JNI; tests that must not touch a file.
pub struct SnapshotStore {
    inner: std::sync::Mutex<SnapshotInner>,
}

struct SnapshotInner {
    settings: Settings,
    profiles: Vec<(String, String)>,
    known_hosts: KnownHosts,
    /// Bumped on every `save`. The host compares against what it last persisted.
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

    /// Replace what the shell will load next. Does not bump `saved_gen`.
    pub fn set(&self, settings: Settings) {
        self.inner.lock().unwrap().settings = settings;
    }

    pub fn set_profiles(&self, profiles: Vec<(String, String)>) {
        self.inner.lock().unwrap().profiles = profiles;
    }

    pub fn set_known_hosts(&self, hosts: KnownHosts) {
        self.inner.lock().unwrap().known_hosts = hosts;
    }

    /// Generation only — cheap enough to compare every frame.
    pub fn saved_gen(&self) -> u64 {
        self.inner.lock().unwrap().saved_gen
    }

    /// Persist when the generation moved since the last look.
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
        store.set(Settings::default());
        assert_eq!(store.load().ui_palette, Settings::default().ui_palette);
        assert_eq!(store.snapshot().1, 1);
    }
}
