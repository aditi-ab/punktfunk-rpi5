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
pub(crate) use std::collections::{BTreeMap, HashSet};
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::time::{SystemTime, UNIX_EPOCH};
pub(crate) use utoipa::ToSchema;

mod art;
mod custom;
mod detect;
#[cfg(windows)]
mod epic;
#[cfg(windows)]
mod gog;
#[cfg(target_os = "linux")]
mod heroic;
mod hidden;
mod launch;
#[cfg(target_os = "linux")]
mod lutris;
mod plugin_launch;
mod scanners;
mod steam;
#[cfg(windows)]
mod xbox;

pub use art::*;
pub use custom::*;
pub use detect::*;
#[cfg(windows)]
pub use epic::*;
#[cfg(windows)]
pub use gog::*;
#[cfg(target_os = "linux")]
pub use heroic::*;
pub use hidden::*;
pub use launch::*;
#[cfg(target_os = "linux")]
pub use lutris::*;
pub use plugin_launch::*;
pub use scanners::*;
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

/// Descriptive metadata for a title — everything a richer library UI (details pane, platform
/// filter, couch-pick badges) renders beyond the poster. Every field is optional and defaults to
/// absent, so pre-metadata catalogs, providers, and clients keep working unchanged. The struct is
/// `#[serde(flatten)]`-ed into [`GameEntry`] / the custom-store shapes: one definition, a flat
/// wire shape everywhere.
///
/// Values are free-form display strings, not enums — emulation sources (RomM, EmuDeck, Playnite)
/// each have their own vocabulary and the host has no business normalizing it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
pub struct GameMeta {
    /// The system the title runs on — `"PS2"`, `"Xbox 360"`, `"SNES"`, … Installed-store
    /// scanners stamp `"PC"`; `GET /library?platform=` filters on it (case-insensitive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "PS2")]
    pub platform: Option<String>,
    /// Short blurb for a details pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// Year of first release — the granularity metadata sources reliably agree on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 2001)]
    pub release_year: Option<u16>,
    /// Genre taxonomy from the metadata source (`"RPG"`, `"Platformer"`, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    /// Free-form organizational labels (`"co-op"`, `"kids"`, `"finished"`, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Release region — emulation-relevant (`"NTSC-U"`, `"PAL"`, `"NTSC-J"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Maximum simultaneous (local) players.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub players: Option<u8>,
}

impl GameMeta {
    /// The one field an installed-store scanner can assert about its own titles: they run on this
    /// host, i.e. on a PC. Everything else stays absent (the launchers' local files don't carry it).
    pub(crate) fn pc() -> Self {
        GameMeta {
            platform: Some("PC".into()),
            ..Default::default()
        }
    }
}

/// What a library entry *is* — an ordinary title, or the launcher application itself (Steam Big
/// Picture, Heroic, Playnite fullscreen). Purely a presentation hint: a launcher entry launches,
/// leases and lists exactly like a game (design D4), and clients that don't know the field render it
/// as a plain tile. Serde-default `game` and skip-serialized when default, so the wire is unchanged
/// for every entry that doesn't opt in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum GameRole {
    /// An ordinary title.
    #[default]
    Game,
    /// The launcher application itself.
    Launcher,
}

impl GameRole {
    /// Whether this is the serde default (`game`) — the `skip_serializing_if` predicate that keeps
    /// the field off the wire for the overwhelming majority of entries.
    pub(crate) fn is_game(&self) -> bool {
        matches!(self, Self::Game)
    }
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
    /// Whether this entry is a game or the launcher itself — see [`GameRole`].
    #[serde(default, skip_serializing_if = "GameRole::is_game")]
    pub role: GameRole,
    /// How the host would launch it, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchSpec>,
    /// The external provider owning this entry (custom-store entries synced by a provider
    /// plugin, RFC §8) — `None` for installed-store titles and manual custom entries. The
    /// console uses it for attribution; `GET /library?provider=` filters on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// How to recognize this title's process(es) once it is running ([`DetectSpec`]) — filled in by
    /// each provider from paths it already read while scanning.
    ///
    /// **Host-internal: never serialized.** It names local filesystem paths, so it stays out of both
    /// the catalog JSON the client renders and the OpenAPI schema; it rides here only so the
    /// providers that already hold this data don't have to be re-scanned.
    #[serde(skip)]
    #[schema(ignore)]
    pub detect: DetectSpec,
    /// Descriptive metadata, flattened — see [`GameMeta`].
    #[serde(flatten)]
    pub meta: GameMeta,
}

/// A library entry plus the operator's own view of it — today, whether they hid it.
///
/// A separate type rather than a field on [`GameEntry`] for two reasons. It keeps the visibility
/// answer out of the providers entirely: a store parser has no opinion on what the operator hid, and
/// adding `hidden: false` to all eight construction sites would imply it does. More importantly it
/// makes the lane rule a TYPE guarantee instead of a discipline — `GET /library` answers
/// `Vec<GameEntry>` on every lane but the operator's, so a hidden entry cannot leak to a paired
/// client by someone forgetting a filter; there is no field there to leak.
///
/// `flatten` keeps the wire shape identical to a plain entry with one extra key, so the console
/// parses one model either way.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct OperatorGameEntry {
    #[serde(flatten)]
    pub entry: GameEntry,
    /// The operator hid this title ([`set_entry_hidden`]) — omitted when false, so the shape only
    /// grows for entries that actually are hidden.
    #[serde(skip_serializing_if = "is_not_hidden")]
    pub hidden: bool,
}

/// `skip_serializing_if` predicate for [`OperatorGameEntry::hidden`] — `&bool` as serde requires.
fn is_not_hidden(hidden: &bool) -> bool {
    !*hidden
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

/// The full library: every *enabled* source's titles merged + the custom entries, sorted by title.
///
/// Two independent gates run here, both at READ time so neither ever mutates stored state:
///
/// * **The operator's source toggles** (`scanners.rs`, persisted as a disabled-set in
///   `library-scanners.json`) hide a source's titles from every surface — this grid, native clients,
///   `/applist`, and launch resolution. They apply to built-in scanners *and* to plugin sources,
///   which is what lets one toggle keep working verbatim across the whole migration: the ids match
///   (provider id = claimed store id = old scanner id).
/// * **Store claims** (D2): while a library plugin holds a store's claim, the matching built-in
///   scanner is skipped so the two never double-list the same titles during the bridge releases.
///   Removing the plugin releases the claim and the built-in comes straight back.
///
/// The user-curated custom store is not a source and always contributes.
///
/// A **third** gate rides on top of these two: the operator's per-entry hides (`hidden.rs`). It is
/// applied here rather than at each call site so a hidden title is gone from every surface by
/// construction — the grid, native clients, `/applist`, and launch resolution — exactly as a
/// disabled source's titles are. [`all_games_for_operator`] is the single deliberate exception.
pub fn all_games() -> Vec<GameEntry> {
    let hidden = hidden_ids();
    let mut games = collect_games();
    games.retain(|g| !hidden.contains(&g.id));
    games
}

/// The library **including** the operator's hidden titles, each flagged.
///
/// The console's list is the only caller, and only on the operator's own lane (`GET /library`
/// branches on it): a hidden entry has to be visible SOMEWHERE or it could never be brought back.
/// Everything else — every paired client, the GameStream app list, launch resolution — goes through
/// [`all_games`] and never sees them.
pub fn all_games_for_operator() -> Vec<OperatorGameEntry> {
    let hidden = hidden_ids();
    collect_games()
        .into_iter()
        .map(|entry| OperatorGameEntry {
            hidden: hidden.contains(&entry.id),
            entry,
        })
        .collect()
}

/// Merge every enabled source + the custom entries, sorted by title — with no visibility gate of its
/// own. Split out so the two public views above cannot drift: they differ only in what they do with
/// the hidden set, never in what they collect.
fn collect_games() -> Vec<GameEntry> {
    let off = disabled_scanners();
    let claimed = claimed_stores();
    // A built-in scanner runs when the operator hasn't disabled it AND no plugin has claimed its
    // store out from under it.
    let on = |id: &str| !off.contains(id) && !claimed.contains_key(id);
    let mut games = Vec::new();
    if on("steam") {
        games.extend(SteamProvider.list());
    }
    // The Lutris + Heroic providers are Linux-only (their launchers are); on other hosts the library
    // is Steam + custom. Each provider is best-effort (empty when its store isn't present).
    #[cfg(target_os = "linux")]
    {
        if on("lutris") {
            games.extend(LutrisProvider.list());
        }
        if on("heroic") {
            games.extend(HeroicProvider.list());
        }
    }
    // Windows store providers (their launchers are Windows-only): Epic + GOG + Xbox/Game Pass.
    #[cfg(windows)]
    {
        if on("epic") {
            games.extend(EpicProvider.list());
        }
        if on("gog") {
            games.extend(GogProvider.list());
        }
        if on("xbox") {
            games.extend(XboxProvider.list());
        }
    }
    // Stored entries: manual ones always contribute; a provider's are subject to the same source
    // toggle a built-in scanner is (WP2.6). The plugin may keep reconciling while it is off — the
    // entries stay stored and simply aren't surfaced, exactly like a disabled scanner's titles.
    games.extend(
        load_custom()
            .into_iter()
            .filter(|e| !source_id_for(e).is_some_and(|src| off.contains(src)))
            .map(GameEntry::from),
    );
    games.sort_by_key(|g| g.title.to_lowercase());
    games
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, title: &str) -> GameEntry {
        GameEntry {
            id: id.into(),
            store: id.split_once(':').map_or("custom", |(s, _)| s).into(),
            title: title.into(),
            art: Artwork::default(),
            role: GameRole::default(),
            launch: None,
            provider: None,
            detect: DetectSpec::default(),
            meta: GameMeta::default(),
        }
    }

    /// The console codes against this shape, so pin it: the operator view must be a normal entry
    /// with ONE extra key, and that key must vanish when the title is visible.
    ///
    /// The skip matters beyond tidiness — it is what keeps this response byte-identical to the old
    /// one for a library with nothing hidden, so shipping the feature cannot change what an existing
    /// console renders until someone actually hides something.
    #[test]
    fn operator_entry_flattens_and_omits_hidden_when_false() {
        let visible = OperatorGameEntry {
            entry: entry("steam:70", "Half-Life"),
            hidden: false,
        };
        let v = serde_json::to_value(&visible).expect("serializes");
        assert_eq!(v["id"], "steam:70", "the entry's fields stay at top level");
        assert_eq!(v["title"], "Half-Life");
        assert!(
            v.get("hidden").is_none(),
            "a visible entry must not carry the key at all: {v}"
        );

        let hidden = OperatorGameEntry {
            entry: entry("steam:70", "Half-Life"),
            hidden: true,
        };
        let v = serde_json::to_value(&hidden).expect("serializes");
        assert_eq!(v["hidden"], true);
        assert_eq!(v["id"], "steam:70", "flatten still applies when hidden");
    }

    /// `all_games` and `all_games_for_operator` must agree on WHICH entries exist and differ only in
    /// visibility — they share `collect_games` for exactly that reason. This pins the shared-source
    /// property the same way the art test pins write/read symmetry: both views of an id-set built
    /// from one collector, so a future edit that inlines one of them is caught.
    #[test]
    fn hidden_filter_is_the_only_difference_between_the_two_views() {
        let games = vec![
            entry("steam:70", "Half-Life"),
            entry("lutris:4", "Syndicate"),
            entry("custom:abc", "Chrono Trigger"),
        ];
        let hidden: HashSet<String> = ["lutris:4".to_string()].into_iter().collect();

        let operator: Vec<OperatorGameEntry> = games
            .iter()
            .cloned()
            .map(|entry| OperatorGameEntry {
                hidden: hidden.contains(&entry.id),
                entry,
            })
            .collect();
        let played: Vec<GameEntry> = games
            .into_iter()
            .filter(|g| !hidden.contains(&g.id))
            .collect();

        assert_eq!(operator.len(), 3, "the operator sees every title");
        assert_eq!(played.len(), 2, "a player does not see the hidden one");
        assert!(
            !played.iter().any(|g| g.id == "lutris:4"),
            "the hidden id must be absent, not merely flagged"
        );
        assert_eq!(
            operator.iter().filter(|r| r.hidden).count(),
            1,
            "exactly the hidden one is flagged"
        );
    }
}
