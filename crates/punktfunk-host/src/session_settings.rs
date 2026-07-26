//! Session⇄game lifetime settings (`<config>/session-settings.json`).
//!
//! Two operator choices, both about what a session and its game owe each other
//! (design/session-game-lifetime.md §8):
//!
//! * [`SessionSettings::session_on_game_exit`] — end the streaming session when the launched game
//!   exits, so the client returns to its library instead of a bare desktop. **On by default**: it is
//!   what a player expects, and it is already the shipped behavior for dedicated game sessions.
//! * [`SessionSettings::game_on_session_end`] — end the launched game when the session ends.
//!   **Off by default** ([`GameOnSessionEnd::Keep`]), because ending a game can cost unsaved
//!   progress. `Always` additionally waits out [`SessionSettings::disconnect_grace_seconds`] so a
//!   network blip never costs a save.
//!
//! Deliberately its own small store rather than more axes on the display policy: keep-alive is about
//! how long a *display* survives a disconnect (default 10 s), while this is about a *game* and the
//! player's unsaved progress (default 5 minutes). Sharing one number would force one of them wrong.
//!
//! Storage follows the same shape as `DisplayPolicyStore` / `pf_gpu::GpuPrefStore`: a missing **or**
//! corrupt file means "unconfigured" with a warning (a settings file must never fail host startup),
//! writes are private-dir + temp + atomic rename, and the in-memory value changes only after the
//! write lands.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use utoipa::ToSchema;

/// What to do with the launched game when its session ends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GameOnSessionEnd {
    /// Leave it running (the historical behavior, and the default — nothing is ever killed unless
    /// the operator asks for it).
    #[default]
    Keep,
    /// End it when the client stops the session deliberately, but leave it running if the client
    /// merely vanished — so a dropped connection stays reconnectable.
    OnQuit,
    /// End it whenever the session ends. A deliberate stop ends it at once; a drop starts the
    /// reconnect window and ends it only if nobody comes back.
    Always,
}

impl GameOnSessionEnd {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::OnQuit => "on_quit",
            Self::Always => "always",
        }
    }
}

/// The default reconnect window before `Always` ends a game — long enough to cover a Wi-Fi
/// roam, a client crash-and-restart, or a walk to another room, because the cost of being wrong is
/// the player's unsaved progress.
const DEFAULT_GRACE_SECS: u32 = 300;
/// Clamp bounds for the window. The floor keeps a mis-typed `1` from making `Always` behave like an
/// instant kill; the ceiling (24 h) keeps a stray large value from pinning a lease forever.
const MIN_GRACE_SECS: u32 = 10;
const MAX_GRACE_SECS: u32 = 86_400;

fn default_grace() -> u32 {
    DEFAULT_GRACE_SECS
}

fn default_true() -> bool {
    true
}

fn one() -> u32 {
    1
}

/// The persisted settings.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct SessionSettings {
    #[serde(default = "one")]
    pub version: u32,
    /// End the launched game when the session ends. See [`GameOnSessionEnd`].
    #[serde(default)]
    pub game_on_session_end: GameOnSessionEnd,
    /// End the streaming session when the launched game exits.
    #[serde(default = "default_true")]
    pub session_on_game_exit: bool,
    /// How long a vanished client has to reconnect before `Always` ends its game. Ignored by the
    /// other two policies.
    #[serde(default = "default_grace")]
    pub disconnect_grace_seconds: u32,
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            version: 1,
            game_on_session_end: GameOnSessionEnd::default(),
            session_on_game_exit: true,
            disconnect_grace_seconds: DEFAULT_GRACE_SECS,
        }
    }
}

impl SessionSettings {
    /// Clamp anything a hand-edited file (or a plugin) could get wrong.
    pub fn sanitized(mut self) -> Self {
        self.version = 1;
        self.disconnect_grace_seconds = self
            .disconnect_grace_seconds
            .clamp(MIN_GRACE_SECS, MAX_GRACE_SECS);
        self
    }
}

/// The store: the loaded file value (`None` when no file exists) behind its path.
pub struct SessionSettingsStore {
    path: PathBuf,
    cur: Mutex<Option<SessionSettings>>,
}

impl SessionSettingsStore {
    /// Load from `path`. Missing ⇒ unconfigured; corrupt ⇒ unconfigured **with a warning**, never a
    /// startup failure.
    pub fn load_from(path: PathBuf) -> Self {
        let cur = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<SessionSettings>(&bytes) {
                Ok(s) => Some(s.sanitized()),
                Err(e) => {
                    tracing::warn!(path = %path.display(),
                        "session-settings.json unreadable — using built-in defaults: {e}");
                    None
                }
            },
            Err(_) => None,
        };
        Self {
            path,
            cur: Mutex::new(cur),
        }
    }

    /// The stored settings, or the defaults when unconfigured.
    pub fn get(&self) -> SessionSettings {
        self.cur
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default()
    }

    /// Whether an operator has ever configured this host (drives the console's "using defaults" hint).
    pub fn configured(&self) -> bool {
        self.cur.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    /// Persist + adopt (sanitized first). Memory changes only if the write lands, so a full disk
    /// can't leave the running host disagreeing with its own file.
    pub fn set(&self, settings: SessionSettings) -> Result<()> {
        let settings = settings.sanitized();
        if let Some(dir) = self.path.parent() {
            pf_paths::create_private_dir(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        pf_paths::write_secret_file(&tmp, &serde_json::to_vec_pretty(&settings)?)?;
        std::fs::rename(&tmp, &self.path)?;
        *self.cur.lock().unwrap_or_else(|e| e.into_inner()) = Some(settings);
        Ok(())
    }
}

/// The process-wide store, loaded once on first access — the same global-accessor shape as
/// `vdisplay::prefs()` / `pf_gpu::prefs()`, because the lifetime decisions happen deep in the session
/// teardown path where no app state is threaded.
pub fn store() -> &'static SessionSettingsStore {
    static STORE: OnceLock<SessionSettingsStore> = OnceLock::new();
    STORE.get_or_init(|| {
        SessionSettingsStore::load_from(pf_paths::config_dir().join("session-settings.json"))
    })
}

/// The effective settings (shorthand for `store().get()`).
pub fn get() -> SessionSettings {
    store().get()
}

/// Which lifetime axes this build actually acts on, for the console to grey out the rest. Both
/// directions need a launch path and a way to see processes; macOS has neither today.
pub fn enforced() -> Vec<String> {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        vec![
            "session_on_game_exit".to_string(),
            "game_on_session_end".to_string(),
            "disconnect_grace_seconds".to_string(),
        ]
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_ones() {
        let d = SessionSettings::default();
        // End-session-on-game-exit ships ON; ending games ships OFF.
        assert!(d.session_on_game_exit);
        assert_eq!(d.game_on_session_end, GameOnSessionEnd::Keep);
        assert_eq!(d.disconnect_grace_seconds, 300);
    }

    #[test]
    fn an_empty_object_decodes_to_the_defaults() {
        // Every field is `#[serde(default)]`, so a file written by an older/partial writer keeps
        // working instead of failing the load.
        let s: SessionSettings = serde_json::from_str("{}").expect("empty object");
        assert!(s.session_on_game_exit);
        assert_eq!(s.game_on_session_end, GameOnSessionEnd::Keep);
        assert_eq!(s.disconnect_grace_seconds, 300);
        assert_eq!(s.version, 1);
    }

    #[test]
    fn policy_names_are_snake_case_on_the_wire() {
        // These strings are the API contract with the console.
        let json = serde_json::to_string(&GameOnSessionEnd::OnQuit).unwrap();
        assert_eq!(json, "\"on_quit\"");
        let back: GameOnSessionEnd = serde_json::from_str("\"always\"").unwrap();
        assert_eq!(back, GameOnSessionEnd::Always);
        assert_eq!(GameOnSessionEnd::Keep.as_str(), "keep");
    }

    #[test]
    fn grace_is_clamped_both_ways() {
        let low = SessionSettings {
            disconnect_grace_seconds: 0,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(low.disconnect_grace_seconds, MIN_GRACE_SECS);
        let high = SessionSettings {
            disconnect_grace_seconds: u32::MAX,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(high.disconnect_grace_seconds, MAX_GRACE_SECS);
    }

    #[test]
    fn missing_file_is_unconfigured_and_corrupt_file_does_not_fail() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("session-settings.json");
        let store = SessionSettingsStore::load_from(path.clone());
        assert!(!store.configured());
        assert_eq!(store.get().game_on_session_end, GameOnSessionEnd::Keep);

        std::fs::write(&path, b"{ not json").unwrap();
        let store = SessionSettingsStore::load_from(path.clone());
        assert!(
            !store.configured(),
            "corrupt file must read as unconfigured"
        );
        assert!(store.get().session_on_game_exit, "defaults still apply");
    }

    #[test]
    fn set_persists_sanitized_and_round_trips() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("session-settings.json");
        let store = SessionSettingsStore::load_from(path.clone());
        store
            .set(SessionSettings {
                version: 99,
                game_on_session_end: GameOnSessionEnd::Always,
                session_on_game_exit: false,
                disconnect_grace_seconds: 5, // below the floor
            })
            .expect("write");
        assert!(store.configured());
        let got = store.get();
        assert_eq!(got.game_on_session_end, GameOnSessionEnd::Always);
        assert!(!got.session_on_game_exit);
        assert_eq!(got.disconnect_grace_seconds, MIN_GRACE_SECS);
        assert_eq!(got.version, 1, "version is normalized, not echoed");
        // A fresh load sees the same thing, and no temp file is left behind.
        let reloaded = SessionSettingsStore::load_from(path.clone());
        assert_eq!(reloaded.get().game_on_session_end, GameOnSessionEnd::Always);
        assert!(!path.with_extension("json.tmp").exists());
    }
}
