//! Client settings profiles — named bundles of setting overrides applied on top of the
//! global [`Settings`] (design/client-settings-profiles.md §4).
//!
//! A profile overrides only the fields the user touched; everything else keeps following the
//! global defaults *live*, so fixing a global once fixes it everywhere. That is why an overlay
//! is sparse `Option`s rather than a snapshot copy, and why `Some(x)` is written on touch and
//! `None` on an explicit "reset to default" — never by diffing against the current global (a
//! `Some` equal to today's global is a legitimate *pin*: the profile keeps `x` when the global
//! later moves).
//!
//! The catalog lives in its own `client-profiles.json` beside the settings file, deliberately
//! NOT inside it: the settings file has five whole-file load-modify-save writers (two shells,
//! the console settings screen, the session's resize callback, Decky) with no merge, so a
//! profile written by one would be dropped by the next. This file is touched only by
//! profile-aware code, and written temp+rename.
//!
//! Which host uses which profile is a field on the host record ([`crate::trust::KnownHost`]),
//! not a map keyed here — see §4.1 of the design for why the catalog owns no host keys.

use crate::trust::{config_dir, write_atomic, Settings, StatsVerbosity};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The catalog file's schema version. Bumped only for a breaking shape change — additive
/// fields ride the unknown-key preservation below.
pub const PROFILES_VERSION: u32 = 1;

/// Every profileable ("tier P") setting, as `Option<T>`: `None` = inherit the global value,
/// live. Tier-H fields (host properties like `clipboard_sync`) and tier-G ones (this
/// device's hardware/endpoints) are deliberately absent — see the design's §3 curation.
///
/// `extra` preserves keys this build doesn't know (a newer client's tier-P field): the
/// don't-clobber rule that already governs the GTK `ChoiceRow` pickers extends to the whole
/// overlay — opening and saving a profile on an older client must not erase what a newer one
/// stored.
#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SettingsOverlay {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_hz: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_window: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render_scale: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdr_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compositor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_channels: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mic_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub touch_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mouse_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invert_scroll: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inhibit_shortcuts: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gamepad: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats_verbosity: Option<StatsVerbosity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fullscreen_on_stream: Option<bool>,
    /// Overlay keys a newer client wrote and this one doesn't model — carried through a
    /// load→save round-trip untouched.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl SettingsOverlay {
    /// The one resolution seam: this overlay on top of `base`. Pure — no store reads, no
    /// clock, no environment — so it is fully testable field by field.
    pub fn apply(&self, base: &Settings) -> Settings {
        let mut s = base.clone();
        if let Some(v) = self.width {
            s.width = v;
        }
        if let Some(v) = self.height {
            s.height = v;
        }
        if let Some(v) = self.refresh_hz {
            s.refresh_hz = v;
        }
        if let Some(v) = self.match_window {
            s.match_window = v;
        }
        if let Some(v) = self.bitrate_kbps {
            s.bitrate_kbps = v;
        }
        if let Some(v) = self.render_scale {
            s.render_scale = v;
        }
        if let Some(v) = &self.codec {
            s.codec = v.clone();
        }
        if let Some(v) = self.hdr_enabled {
            s.hdr_enabled = v;
        }
        if let Some(v) = &self.compositor {
            s.compositor = v.clone();
        }
        if let Some(v) = self.audio_channels {
            s.audio_channels = v;
        }
        if let Some(v) = self.mic_enabled {
            s.mic_enabled = v;
        }
        if let Some(v) = &self.touch_mode {
            s.touch_mode = v.clone();
        }
        if let Some(v) = &self.mouse_mode {
            s.mouse_mode = v.clone();
        }
        if let Some(v) = self.invert_scroll {
            s.invert_scroll = v;
        }
        if let Some(v) = self.inhibit_shortcuts {
            s.inhibit_shortcuts = v;
        }
        if let Some(v) = &self.gamepad {
            s.gamepad = v.clone();
        }
        if let Some(v) = self.stats_verbosity {
            // Through the setter so the legacy `show_stats` bool stays coherent for
            // pre-tier binaries reading the same settings file.
            s.set_stats_verbosity(v);
        }
        if let Some(v) = self.fullscreen_on_stream {
            s.fullscreen_on_stream = v;
        }
        s
    }

    /// True when the profile overrides nothing — "inherits everything", the state a freshly
    /// created profile starts in. Unknown-key carry-through counts: a profile that only holds
    /// a newer client's field is not empty.
    pub fn is_empty(&self) -> bool {
        *self == SettingsOverlay::default()
    }
}

/// One named bundle of overrides. `id` is stable across renames — bindings, pins and deep
/// links all point at it, never at the name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamProfile {
    pub id: String,
    /// User-facing and editable; unique case-insensitively (menus are ambiguous otherwise —
    /// enforced by the editing UIs via [`ProfilesFile::name_taken`]).
    pub name: String,
    /// `#RRGGBB` chip color. The UI may ignore it; the schema reserves it (pinned cards use
    /// it to tint their subtitle).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    #[serde(default)]
    pub overrides: SettingsOverlay,
    /// Profile keys a newer client wrote — preserved across a load→save round-trip.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl StreamProfile {
    /// A new, empty profile: inherits everything (the right creation default under
    /// inherit-by-exception — "Duplicate" covers starting from another profile).
    pub fn new(name: impl Into<String>) -> StreamProfile {
        StreamProfile {
            id: new_profile_id(),
            name: name.into(),
            accent: None,
            overrides: SettingsOverlay::default(),
            extra: BTreeMap::new(),
        }
    }
}

/// What a `profile=` / `--profile` reference resolved to. Ambiguity is reported rather than
/// guessed: a link or flag naming two profiles must refuse, not pick one (design
/// client-deep-links.md §8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    Found,
    NotFound,
    /// More than one profile carries this name (case-insensitively).
    Ambiguous,
}

/// The profile catalog — client-wide, not per host: "Work" applied to three hosts is one
/// profile, and the per-host part is only the binding on the host record.
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct ProfilesFile {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub profiles: Vec<StreamProfile>,
}

impl ProfilesFile {
    pub fn path() -> anyhow::Result<PathBuf> {
        Ok(config_dir()?.join("client-profiles.json"))
    }

    /// The stored catalog, or an empty one — a missing or unreadable file is "no profiles",
    /// never an error: nothing about streaming may hinge on this file existing.
    pub fn load() -> ProfilesFile {
        Self::path()
            .and_then(|p| Ok(std::fs::read_to_string(p)?))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist temp+rename, so a crash or a full disk mid-write leaves the previous catalog
    /// intact instead of a truncated one.
    pub fn save(&mut self) -> anyhow::Result<()> {
        self.version = PROFILES_VERSION;
        let p = Self::path()?;
        std::fs::create_dir_all(p.parent().unwrap())?;
        write_atomic(&p, &serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn find_by_id(&self, id: &str) -> Option<&StreamProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// Resolve a reference the way every surface must: exact id first, then a unique
    /// case-insensitive name. Ambiguous names resolve to [`Resolution::Ambiguous`], never to
    /// the first match.
    pub fn resolve(&self, reference: &str) -> (Option<&StreamProfile>, Resolution) {
        if let Some(p) = self.find_by_id(reference) {
            return (Some(p), Resolution::Found);
        }
        let mut hits = self
            .profiles
            .iter()
            .filter(|p| p.name.eq_ignore_ascii_case(reference));
        match (hits.next(), hits.next()) {
            (Some(p), None) => (Some(p), Resolution::Found),
            (Some(_), Some(_)) => (None, Resolution::Ambiguous),
            _ => (None, Resolution::NotFound),
        }
    }

    /// Is this name already used (case-insensitively) by a *different* profile? The
    /// create/rename guard — `except` is the profile being renamed, so renaming "Work" to
    /// "work" is allowed.
    pub fn name_taken(&self, name: &str, except: Option<&str>) -> bool {
        self.profiles
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(name) && Some(p.id.as_str()) != except)
    }
}

/// 12 lowercase hex chars — the `library::new_id` shape, minted from the OS RNG (no uuid
/// dependency, no collision in any realistic catalog).
pub fn new_profile_id() -> String {
    let b: [u8; 6] = rand::random();
    hex_lower(&b)
}

/// A random UUID-v4 in the canonical 8-4-4-4-12 form — the stable host-record identity
/// (design §4.5). Matches the shape Apple's `StoredHost.id` already has, so a deep link's
/// host-ref grammar is one format on every platform.
pub fn new_record_uuid() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    let h = hex_lower(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The overlay applies field by field: a `Some` wins, a `None` keeps the base's live
    /// value — including values that happen to equal the base (an explicit pin).
    #[test]
    fn overlay_applies_only_what_it_overrides() {
        let base = Settings {
            width: 1920,
            height: 1080,
            bitrate_kbps: 20000,
            codec: "hevc".into(),
            ..Default::default()
        };

        let empty = SettingsOverlay::default();
        let out = empty.apply(&base);
        assert_eq!((out.width, out.height), (1920, 1080));
        assert_eq!(out.bitrate_kbps, 20000);
        assert_eq!(out.codec, "hevc");
        assert!(empty.is_empty());

        let overlay = SettingsOverlay {
            width: Some(3840),
            height: Some(2160),
            refresh_hz: Some(120),
            bitrate_kbps: Some(80000),
            render_scale: Some(1.5),
            codec: Some("av1".into()),
            hdr_enabled: Some(false),
            compositor: Some("gamescope".into()),
            audio_channels: Some(6),
            mic_enabled: Some(true),
            touch_mode: Some("pointer".into()),
            mouse_mode: Some("desktop".into()),
            invert_scroll: Some(true),
            inhibit_shortcuts: Some(false),
            gamepad: Some("dualsense".into()),
            match_window: Some(true),
            fullscreen_on_stream: Some(false),
            stats_verbosity: Some(StatsVerbosity::Detailed),
            ..Default::default()
        };
        assert!(!overlay.is_empty());
        let out = overlay.apply(&base);
        assert_eq!((out.width, out.height, out.refresh_hz), (3840, 2160, 120));
        assert_eq!(out.bitrate_kbps, 80000);
        assert_eq!(out.render_scale, 1.5);
        assert_eq!(out.codec, "av1");
        assert!(!out.hdr_enabled);
        assert_eq!(out.compositor, "gamescope");
        assert_eq!(out.audio_channels, 6);
        assert!(out.mic_enabled);
        assert_eq!(out.touch_mode, "pointer");
        assert_eq!(out.mouse_mode, "desktop");
        assert!(out.invert_scroll);
        assert!(!out.inhibit_shortcuts);
        assert_eq!(out.gamepad, "dualsense");
        assert!(out.match_window);
        assert!(!out.fullscreen_on_stream);
        assert_eq!(out.stats_verbosity(), StatsVerbosity::Detailed);
        // The tier goes through the setter, so the legacy bool a pre-tier binary reads
        // stays coherent with it.
        assert!(out.show_stats);
        // Tier-G/H fields are not in the overlay at all — the device's decoder pick, its
        // audio endpoints and the per-host clipboard decision survive any profile.
        assert_eq!(out.decoder, base.decoder);
        assert_eq!(out.speaker_device, base.speaker_device);

        // An overlay that only carries a value equal to the base is still an override: the
        // profile pins it, so a later global change doesn't move it.
        let pin = SettingsOverlay {
            bitrate_kbps: Some(20000),
            ..Default::default()
        };
        assert!(!pin.is_empty());
        let mut moved = base.clone();
        moved.bitrate_kbps = 50000;
        assert_eq!(pin.apply(&moved).bitrate_kbps, 20000);
    }

    /// Stats verbosity Off must survive `apply` — it is a legitimate override, and going
    /// through `set_stats_verbosity` keeps `show_stats` in sync in that direction too.
    #[test]
    fn overlay_can_turn_the_stats_overlay_off() {
        let mut base = Settings::default();
        base.set_stats_verbosity(StatsVerbosity::Detailed);
        let overlay = SettingsOverlay {
            stats_verbosity: Some(StatsVerbosity::Off),
            ..Default::default()
        };
        let out = overlay.apply(&base);
        assert_eq!(out.stats_verbosity(), StatsVerbosity::Off);
        assert!(!out.show_stats);
    }

    /// A catalog round-trips, and values this build can't represent survive it: an unknown
    /// codec string (a newer client's option) stays as written, and an unknown overlay KEY
    /// is carried through untouched rather than erased — the don't-clobber rule.
    #[test]
    fn catalog_round_trips_and_preserves_what_it_cannot_represent() {
        // `r##` — the accent value below contains a `"#` pair that would close an `r#` literal.
        let stored = r##"{
            "version": 1,
            "profiles": [
                {
                    "id": "a1b2c3d4e5f6",
                    "name": "Game",
                    "accent": "#ff8800",
                    "overrides": {
                        "width": 3840, "height": 2160, "refresh_hz": 120,
                        "codec": "vvc-from-the-future",
                        "some_new_axis": {"nested": true},
                        "stats_verbosity": "compact"
                    },
                    "future_profile_key": 7
                },
                { "id": "0f0f0f0f0f0f", "name": "Work" }
            ]
        }"##;
        let file: ProfilesFile = serde_json::from_str(stored).unwrap();
        assert_eq!(file.profiles.len(), 2);
        let game = file.find_by_id("a1b2c3d4e5f6").unwrap();
        assert_eq!(game.accent.as_deref(), Some("#ff8800"));
        assert_eq!(game.overrides.codec.as_deref(), Some("vvc-from-the-future"));
        assert_eq!(
            game.overrides.stats_verbosity,
            Some(StatsVerbosity::Compact)
        );
        // A profile with no `overrides` key at all is the empty (inherit-everything) one.
        assert!(file
            .find_by_id("0f0f0f0f0f0f")
            .unwrap()
            .overrides
            .is_empty());

        let text = serde_json::to_string(&file).unwrap();
        assert!(text.contains("vvc-from-the-future"));
        assert!(text.contains("some_new_axis"));
        assert!(text.contains("future_profile_key"));
        // Absent overrides serialize away entirely — the file stays readable.
        assert!(!text.contains("null"));
        let round: ProfilesFile = serde_json::from_str(&text).unwrap();
        let game = round.find_by_id("a1b2c3d4e5f6").unwrap();
        assert_eq!(game.overrides.width, Some(3840));
        assert_eq!(game.overrides.extra.len(), 1);
        assert_eq!(game.extra.len(), 1);

        // The unknown value still applies: the session hands the string to the host, which
        // is the component that decides what it can encode.
        let applied = game.overrides.apply(&Settings::default());
        assert_eq!(applied.codec, "vvc-from-the-future");
    }

    /// Resolution order: id first, then a unique case-insensitive name; two profiles sharing
    /// a name resolve to Ambiguous (the caller refuses) rather than to whichever came first.
    #[test]
    fn resolve_prefers_ids_and_refuses_ambiguity() {
        let file = ProfilesFile {
            version: 1,
            profiles: vec![
                StreamProfile {
                    id: "111111111111".into(),
                    name: "Work".into(),
                    ..StreamProfile::new("")
                },
                StreamProfile {
                    id: "222222222222".into(),
                    name: "work".into(),
                    ..StreamProfile::new("")
                },
                StreamProfile {
                    id: "333333333333".into(),
                    name: "Game".into(),
                    ..StreamProfile::new("")
                },
            ],
        };
        assert_eq!(file.resolve("111111111111").1, Resolution::Found);
        assert_eq!(file.resolve("Work").1, Resolution::Ambiguous);
        assert_eq!(file.resolve("game").1, Resolution::Found);
        assert_eq!(file.resolve("GAME").0.unwrap().id, "333333333333");
        assert_eq!(file.resolve("nope").1, Resolution::NotFound);
        assert_eq!(file.resolve("").1, Resolution::NotFound);

        // Duplicate-name guard, and the rename-in-place exemption.
        assert!(file.name_taken("GAME", None));
        assert!(!file.name_taken("GAME", Some("333333333333")));
        assert!(file.name_taken("GAME", Some("111111111111")));
        assert!(!file.name_taken("Travel", None));
    }

    /// Minted ids have the documented shapes and don't repeat.
    #[test]
    fn minted_ids_are_well_formed() {
        let a = new_profile_id();
        assert_eq!(a.len(), 12);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        assert_ne!(a, new_profile_id());

        let u = new_record_uuid();
        assert_eq!(u.len(), 36);
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(u.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
        assert_eq!(parts[2].as_bytes()[0], b'4'); // version nibble
        assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'));
        assert_ne!(u, new_record_uuid());
    }
}
