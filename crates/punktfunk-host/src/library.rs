//! Game library (plan: "surface the user's games"). One uniform [`GameEntry`] grid over every
//! source of titles on this host, so a client never has to know which launcher a title came from.
//!
//! **Every source is a plugin.** The host itself scans nothing: library plugins (Steam, Lutris,
//! Heroic, Epic, GOG, Xbox, Playnite, ROM managers, …) reconcile their titles into the stored
//! catalog over the provider API, each claiming its store so its entries keep the stable
//! `<store>:<external_id>` ids everything downstream already pins (`custom.rs`, design D2). The
//! user-curated **custom** store — entries the operator typed in via the management API / web
//! console — lives in the same catalog and is the one source that is not a plugin.
//!
//! Until v0.28.0 the host also carried six built-in scanners that read the launchers' local files
//! directly. They were the bridge while the plugins were written; the plugins are the product now,
//! and the scanners are gone. What survives them is deliberate: the entry model here, the whole of
//! `launch.rs` (a plugin publishes a validated *value*, the host builds the command — design D1),
//! and the source toggles in `scanners.rs`, whose ids match the claims by construction so an
//! operator's disabled state carried across the extraction untouched.
//!
//! Artwork rides on the entries themselves — a plugin supplies URLs or local files, and the host's
//! art proxy serves the local ones ([`art`]) so a client never receives an unreachable `C:\…` path.
//!
//! This module is read-mostly metadata; *launching* a chosen title (mapping [`LaunchSpec`] onto a
//! gamescope session) is `launch.rs`.

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
mod hidden;
mod launch;
mod plugin_launch;
mod scanners;

pub use art::*;
pub use custom::*;
pub use detect::*;
pub use hidden::*;
pub use launch::*;
pub use plugin_launch::*;
pub use scanners::*;

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

/// The longest an [`GameEntry::icon`] token may be. Generous for a slug like `epic-games`, short
/// enough that the field can never carry a payload.
const ICON_TOKEN_MAX: usize = 32;

/// Whether `t` is a well-formed brand-icon token: `[a-z][a-z0-9-]{0,31}`.
///
/// The host validates the **shape** and nothing else. Which tokens actually draw is a client
/// question — every client ships its own curated set of marks (`assets/launcher-icons`) and falls
/// back to the launcher's name for one it doesn't have, so a host that gated on a registry would
/// only be able to reject tokens that a *newer* client already knows how to draw.
///
/// The shape check is not cosmetic. Without it the field is free-form text a plugin controls, and
/// every client interpolates it — into a resource name, an asset-catalog lookup, a file path. A slug
/// alphabet makes `../`, a URL, a `data:` payload and a NUL unrepresentable, so no client has to
/// re-derive that guard for itself.
pub fn is_icon_token(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= ICON_TOKEN_MAX
        && t.starts_with(|c: char| c.is_ascii_lowercase())
        && t.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Reject a malformed [`GameEntry::icon`] token, naming the offender. `Ok(())` when absent.
pub fn validate_icon(icon: Option<&str>) -> std::result::Result<(), String> {
    match icon {
        Some(t) if !is_icon_token(t) => Err(format!(
            "`icon` must be a brand token matching [a-z][a-z0-9-]{{0,{}}} (got {t:?})",
            ICON_TOKEN_MAX - 1
        )),
        _ => Ok(()),
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
    /// Which brand mark to draw for this entry, as a **token** — `steam`, `heroic`, `playnite` —
    /// never image bytes and never a URL. See [`is_icon_token`].
    ///
    /// It exists for launcher tiles, which by design ship no cover art: a launcher's own icon is
    /// square, every client cover-crops a 2:3 poster, and the crop turns a mark into a strip — so
    /// until now those tiles were the launcher's name on a flat accent face. The token lets a client
    /// draw the real mark from art it already ships, at whatever size its tile happens to be.
    ///
    /// A token rather than art on the wire because the host's art proxy serves *raster* bytes only
    /// ([`art::local_art_bytes`] sniffs the container and refuses anything else, SVG very much
    /// included — it is script-capable XML and the console renders art in a browser). Sending the
    /// name of a mark instead of the mark keeps that refusal intact, keeps the glyph vector at every
    /// tile size, and lets it take the tile's ink.
    ///
    /// Ordinary titles may carry one too — nothing here is launcher-specific — but nothing sets it
    /// for them: a game has real cover art, which is strictly better than a brand mark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "steam")]
    pub icon: Option<String>,
    /// How the host would launch it, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchSpec>,
    /// The external provider owning this entry (entries synced by a provider plugin, RFC §8) —
    /// `None` only for the manual entries the operator typed in. The console uses it for
    /// attribution; `GET /library?provider=` filters on it.
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

/// Which of the four [`Artwork`] fields an art request names — the `<kind>` in
/// `GET /library/art/<id>/<kind>`, and the preference order the GameStream cover proxy walks.
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
}

/// The full library: every *enabled* source's titles, sorted by title.
///
/// Two gates run here, both at READ time so neither ever mutates stored state:
///
/// * **The operator's source toggles** (`scanners.rs`, persisted as a disabled-set in
///   `library-scanners.json`) hide a source's titles from every surface — this grid, native clients,
///   `/applist`, and launch resolution. The plugin may keep reconciling while its source is off; the
///   entries stay stored and simply aren't surfaced.
/// * **The operator's per-entry hides** (`hidden.rs`), applied here rather than at each call site so
///   a hidden title is gone from every surface by construction. [`all_games_for_operator`] is the
///   single deliberate exception.
///
/// Manual custom entries — the ones the operator typed in — carry no source and always contribute.
///
/// There is no longer a third gate. Store claims used to suppress the built-in scanner a plugin had
/// taken over; with the built-ins gone there is nothing left to suppress, so a claim now only fixes
/// the ids a provider's entries surface under (`custom.rs::library_id_for`) and names its row in the
/// sources list.
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
    // Manual entries always contribute; a provider's are subject to the operator's source toggle.
    let mut games: Vec<GameEntry> = load_custom()
        .into_iter()
        .filter(|e| !source_id_for(e).is_some_and(|src| off.contains(src)))
        .map(GameEntry::from)
        .collect();
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
            icon: None,
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
