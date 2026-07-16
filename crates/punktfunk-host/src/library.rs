//! Game library (plan: "surface the user's games"). A small adapter layer over the *stores*
//! installed on the host — today **Steam** (read from local files, no API key) and a
//! user-curated **custom** store (CRUD'd via the management API / web console). Every store
//! produces the same [`GameEntry`], so a client renders one uniform grid and never has to know
//! which launcher a title came from. Future stores (Heroic/Epic, GOG, Lutris, EmuDeck) are just
//! more [`LibraryProvider`]s.
//!
//! Artwork is keyed only by Steam appid against the public Steam CDN (no auth) — the client
//! fetches the posters directly. Custom entries carry user-supplied art URLs.
//!
//! This module is read-mostly metadata; *launching* a chosen title (mapping [`LaunchSpec`] onto a
//! gamescope session) is a later step — the launch hint is carried here so that wiring is trivial.

// Shared vocabulary re-exported to the submodules (each is `use super::*`).
pub(crate) use anyhow::{Context, Result};
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use sha2::{Digest, Sha256};
pub(crate) use std::collections::HashSet;
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::time::{SystemTime, UNIX_EPOCH};
pub(crate) use utoipa::ToSchema;

mod art;
mod custom;
#[cfg(windows)]
mod epic;
#[cfg(windows)]
mod gog;
#[cfg(target_os = "linux")]
mod heroic;
mod launch;
#[cfg(target_os = "linux")]
mod lutris;
mod steam;
#[cfg(windows)]
mod xbox;

pub use art::*;
pub use custom::*;
#[cfg(windows)]
pub use epic::*;
#[cfg(windows)]
pub use gog::*;
#[cfg(target_os = "linux")]
pub use heroic::*;
pub use launch::*;
#[cfg(target_os = "linux")]
pub use lutris::*;
pub use steam::*;
#[cfg(windows)]
pub use xbox::*;

/// Cover art for a title. All fields are URLs (the Steam CDN for Steam titles, user-supplied for
/// custom). The client prefers `portrait` for a grid and falls back to `header` when a title has
/// no 600×900 capsule (common for older Steam apps).
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
pub struct Artwork {
    /// Vertical capsule / poster (Steam `library_600x900.jpg`). Best for a grid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portrait: Option<String>,
    /// Wide background (Steam `library_hero.jpg`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hero: Option<String>,
    /// Transparent title logo (Steam `logo.png`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    /// Horizontal header (Steam `header.jpg`) — the universal fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
}

/// How the host would launch a title (consumed by the session launcher in a later step). Kept
/// open-ended so new stores slot in: `steam_appid` → `steam steam://rungameid/<value>`;
/// `command` → run `<value>` nested in a gamescope session.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct LaunchSpec {
    /// `"steam_appid"` or `"command"`.
    #[schema(example = "steam_appid")]
    pub kind: String,
    /// The appid (for `steam_appid`) or the shell command (for `command`).
    pub value: String,
}

/// One title in the unified library, regardless of which store it came from.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct GameEntry {
    /// Stable, store-qualified id: `steam:<appid>` or `custom:<id>`.
    #[schema(example = "steam:570")]
    pub id: String,
    /// Which store surfaced it: `"steam"` or `"custom"`.
    #[schema(example = "steam")]
    pub store: String,
    pub title: String,
    pub art: Artwork,
    /// How the host would launch it, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchSpec>,
    /// The external provider owning this entry (custom-store entries synced by a provider
    /// plugin, RFC §8) — `None` for installed-store titles and manual custom entries. The
    /// console uses it for attribution; `GET /library?provider=` filters on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// A store that contributes titles to the library. The trait is the extension point for future
/// launchers; today only [`SteamProvider`] implements it.
pub trait LibraryProvider {
    /// Stable store id (`"steam"`, …).
    fn store(&self) -> &'static str;
    /// Enumerate installed/owned titles. Best-effort: returns empty (not an error) when the store
    /// isn't present, so one missing launcher never fails the whole library.
    fn list(&self) -> Vec<GameEntry>;
}

/// Steam art, keyed to one of the four [`Artwork`] fields. Newer/recently-updated titles serve
/// their CDN assets from a per-asset-hash path the client can't predict (e.g.
/// `.../apps/<id>/<hash>/header.jpg`), so the flat legacy URL [`steam_art`] guesses 404s for them —
/// [`steam_art_bytes`] is the robust resolver: local Steam cache (exact, no guessing) first, the
/// flat CDN URL as a fallback (still correct for the many titles that haven't been re-hashed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtKind {
    Portrait,
    Hero,
    Logo,
    Header,
}

impl ArtKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "portrait" => Some(Self::Portrait),
            "hero" => Some(Self::Hero),
            "logo" => Some(Self::Logo),
            "header" => Some(Self::Header),
            _ => None,
        }
    }

    /// Filenames Steam itself caches this kind under in `appcache/librarycache/<appid>/<hash>/`,
    /// tried in order (the 2x portrait, when present, is the sharper asset).
    fn local_filenames(self) -> &'static [&'static str] {
        match self {
            Self::Portrait => &["library_600x900_2x.jpg", "library_600x900.jpg"],
            Self::Hero => &["library_hero.jpg"],
            Self::Logo => &["logo.png"],
            // Steam's local cache names the header asset differently from the store CDN's
            // `header.jpg` (see `cdn_filename`).
            Self::Header => &["library_header.jpg"],
        }
    }

    /// The legacy flat-URL filename on the public Steam CDN (works for any title the CDN hasn't
    /// migrated to a per-asset hash path).
    fn cdn_filename(self) -> &'static str {
        match self {
            Self::Portrait => "library_600x900.jpg",
            Self::Hero => "library_hero.jpg",
            Self::Logo => "logo.png",
            Self::Header => "header.jpg",
        }
    }
}

/// The full library: every store's titles merged + the custom entries, sorted by title.
pub fn all_games() -> Vec<GameEntry> {
    let mut games = SteamProvider.list();
    // The Lutris + Heroic providers are Linux-only (their launchers are); on other hosts the library
    // is Steam + custom. Each provider is best-effort (empty when its store isn't present).
    #[cfg(target_os = "linux")]
    {
        games.extend(LutrisProvider.list());
        games.extend(HeroicProvider.list());
    }
    // Windows store providers (their launchers are Windows-only): Epic + GOG + Xbox/Game Pass.
    #[cfg(windows)]
    {
        games.extend(EpicProvider.list());
        games.extend(GogProvider.list());
        games.extend(XboxProvider.list());
    }
    games.extend(load_custom().into_iter().map(GameEntry::from));
    games.sort_by_key(|g| g.title.to_lowercase());
    games
}
