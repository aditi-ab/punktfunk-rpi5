//! Named setting-override bundles applied on top of global [`Settings`].
//!
//! An overlay is sparse `Option`s, not a snapshot. `Some(x)` is written on
//! touch and `None` on an explicit reset — never by diffing against the
//! current global. A `Some` equal to today's global is a pin: the profile
//! keeps `x` when the global later moves.
//!
//! The catalog is `client-profiles.json` beside the settings file, not
//! inside it: settings writers load-modify-save the whole file with no
//! merge. Written temp+rename. Host→profile binding lives on
//! [`crate::trust::KnownHost`], not here.
//!
//! Design: `design/client-settings-profiles.md`.

use crate::trust::{config_dir, write_atomic, Settings, StatsVerbosity};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Bumped only for a breaking shape change; additive fields ride `extra`.
pub const PROFILES_VERSION: u32 = 1;

/// Sparse overlay of profileable (tier-P) settings. `None` inherits the
/// live global. Host properties (tier H) and this device's hardware
/// (tier G) are absent.
///
/// `extra` is the don't-clobber bag: a load→save on an older client must
/// not erase a newer client's unknown key.
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
    pub enable_444: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ten_bit_sdr: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compositor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_channels: Option<u8>,
    /// How the host is streamed, not this device's hardware — that is why
    /// it is profileable rather than tier-G. Shared key across clients.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_host_audio: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mic_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub echo_cancel: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub touch_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mouse_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invert_scroll: Option<bool>,
    /// Whole ring blob: inherit the default, or own the ring and shortcuts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_actions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inhibit_shortcuts: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gamepad: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gamepad_forwarding: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_buttons: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guide_gesture: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats_verbosity: Option<StatsVerbosity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fullscreen_on_stream: Option<bool>,
    /// First-class so a profile authored on any client applies here
    /// instead of riding `extra` unapplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub present_priority: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smooth_buffer: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vsync: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_vrr: Option<bool>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl SettingsOverlay {
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
        if let Some(v) = self.enable_444 {
            s.enable_444 = v;
        }
        if let Some(v) = self.ten_bit_sdr {
            s.ten_bit_sdr = v;
        }
        if let Some(v) = &self.compositor {
            s.compositor = v.clone();
        }
        if let Some(v) = self.audio_channels {
            s.audio_channels = v;
        }
        if let Some(v) = &self.audio_format {
            s.audio_format = v.clone();
        }
        if let Some(v) = self.keep_host_audio {
            s.keep_host_audio = v;
        }
        if let Some(v) = self.mic_enabled {
            s.mic_enabled = v;
        }
        if let Some(v) = self.echo_cancel {
            s.echo_cancel = v;
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
        if let Some(v) = &self.overlay_actions {
            s.overlay_actions = v.clone();
        }
        if let Some(v) = self.inhibit_shortcuts {
            s.inhibit_shortcuts = v;
        }
        if let Some(v) = &self.gamepad {
            s.gamepad = v.clone();
        }
        if let Some(v) = self.gamepad_forwarding {
            s.gamepad_forwarding = v;
        }
        if let Some(v) = &self.system_buttons {
            s.system_buttons = v.clone();
        }
        if let Some(v) = &self.guide_gesture {
            s.guide_gesture = v.clone();
        }
        if let Some(v) = self.stats_verbosity {
            // Through the setter so the legacy `show_stats` bool stays coherent.
            s.set_stats_verbosity(v);
        }
        if let Some(v) = self.fullscreen_on_stream {
            s.fullscreen_on_stream = v;
        }
        if let Some(v) = &self.present_priority {
            s.present_priority = v.clone();
        }
        if let Some(v) = self.smooth_buffer {
            s.smooth_buffer = v;
        }
        if let Some(v) = self.vsync {
            s.vsync = v;
        }
        if let Some(v) = self.allow_vrr {
            s.allow_vrr = v;
        }
        s
    }

    /// Pin every tier-P field that changed between two effective snapshots.
    /// Compare against what the control showed, not globals — equal-to-global
    /// is still a pin. Only adds; removal is `clear`.
    pub fn absorb(&mut self, before: &Settings, after: &Settings) {
        if after.width != before.width {
            self.width = Some(after.width);
        }
        if after.height != before.height {
            self.height = Some(after.height);
        }
        if after.refresh_hz != before.refresh_hz {
            self.refresh_hz = Some(after.refresh_hz);
        }
        if after.match_window != before.match_window {
            self.match_window = Some(after.match_window);
        }
        if after.bitrate_kbps != before.bitrate_kbps {
            self.bitrate_kbps = Some(after.bitrate_kbps);
        }
        if after.render_scale != before.render_scale {
            self.render_scale = Some(after.render_scale);
        }
        if after.codec != before.codec {
            self.codec = Some(after.codec.clone());
        }
        if after.hdr_enabled != before.hdr_enabled {
            self.hdr_enabled = Some(after.hdr_enabled);
        }
        if after.enable_444 != before.enable_444 {
            self.enable_444 = Some(after.enable_444);
        }
        if after.ten_bit_sdr != before.ten_bit_sdr {
            self.ten_bit_sdr = Some(after.ten_bit_sdr);
        }
        if after.compositor != before.compositor {
            self.compositor = Some(after.compositor.clone());
        }
        if after.audio_channels != before.audio_channels {
            self.audio_channels = Some(after.audio_channels);
        }
        if after.audio_format != before.audio_format {
            self.audio_format = Some(after.audio_format.clone());
        }
        if after.keep_host_audio != before.keep_host_audio {
            self.keep_host_audio = Some(after.keep_host_audio);
        }
        if after.mic_enabled != before.mic_enabled {
            self.mic_enabled = Some(after.mic_enabled);
        }
        if after.echo_cancel != before.echo_cancel {
            self.echo_cancel = Some(after.echo_cancel);
        }
        if after.touch_mode != before.touch_mode {
            self.touch_mode = Some(after.touch_mode.clone());
        }
        if after.mouse_mode != before.mouse_mode {
            self.mouse_mode = Some(after.mouse_mode.clone());
        }
        if after.invert_scroll != before.invert_scroll {
            self.invert_scroll = Some(after.invert_scroll);
        }
        if after.overlay_actions != before.overlay_actions {
            self.overlay_actions = Some(after.overlay_actions.clone());
        }
        if after.inhibit_shortcuts != before.inhibit_shortcuts {
            self.inhibit_shortcuts = Some(after.inhibit_shortcuts);
        }
        if after.gamepad != before.gamepad {
            self.gamepad = Some(after.gamepad.clone());
        }
        if after.gamepad_forwarding != before.gamepad_forwarding {
            self.gamepad_forwarding = Some(after.gamepad_forwarding);
        }
        if after.system_buttons != before.system_buttons {
            self.system_buttons = Some(after.system_buttons.clone());
        }
        if after.guide_gesture != before.guide_gesture {
            self.guide_gesture = Some(after.guide_gesture.clone());
        }
        if after.stats_verbosity() != before.stats_verbosity() {
            self.stats_verbosity = Some(after.stats_verbosity());
        }
        if after.fullscreen_on_stream != before.fullscreen_on_stream {
            self.fullscreen_on_stream = Some(after.fullscreen_on_stream);
        }
        if after.present_priority != before.present_priority {
            self.present_priority = Some(after.present_priority.clone());
        }
        if after.smooth_buffer != before.smooth_buffer {
            self.smooth_buffer = Some(after.smooth_buffer);
        }
        if after.vsync != before.vsync {
            self.vsync = Some(after.vsync);
        }
        if after.allow_vrr != before.allow_vrr {
            self.allow_vrr = Some(after.allow_vrr);
        }
    }

    /// Drop one override by serialised field name. `resolution` is the alias
    /// for the width/height/match-window tri-state one control drives.
    pub fn clear(&mut self, field: &str) -> bool {
        match field {
            "resolution" => {
                self.width = None;
                self.height = None;
                self.match_window = None;
            }
            "width" => self.width = None,
            "height" => self.height = None,
            "refresh_hz" => self.refresh_hz = None,
            "match_window" => self.match_window = None,
            "bitrate_kbps" => self.bitrate_kbps = None,
            "render_scale" => self.render_scale = None,
            "codec" => self.codec = None,
            "hdr_enabled" => self.hdr_enabled = None,
            "enable_444" => self.enable_444 = None,
            "ten_bit_sdr" => self.ten_bit_sdr = None,
            "compositor" => self.compositor = None,
            "audio_channels" => self.audio_channels = None,
            "audio_format" => self.audio_format = None,
            "keep_host_audio" => self.keep_host_audio = None,
            "mic_enabled" => self.mic_enabled = None,
            "echo_cancel" => self.echo_cancel = None,
            "touch_mode" => self.touch_mode = None,
            "mouse_mode" => self.mouse_mode = None,
            "invert_scroll" => self.invert_scroll = None,
            "overlay_actions" => self.overlay_actions = None,
            "inhibit_shortcuts" => self.inhibit_shortcuts = None,
            "gamepad" => self.gamepad = None,
            "gamepad_forwarding" => self.gamepad_forwarding = None,
            "system_buttons" => self.system_buttons = None,
            "guide_gesture" => self.guide_gesture = None,
            "stats_verbosity" => self.stats_verbosity = None,
            "fullscreen_on_stream" => self.fullscreen_on_stream = None,
            "present_priority" => self.present_priority = None,
            "smooth_buffer" => self.smooth_buffer = None,
            "vsync" => self.vsync = None,
            "allow_vrr" => self.allow_vrr = None,
            _ => return false,
        }
        true
    }

    /// True when nothing is overridden. Unknown-key carry-through counts:
    /// a profile that only holds a newer client's field is not empty.
    pub fn is_empty(&self) -> bool {
        *self == SettingsOverlay::default()
    }
}

/// Named override bundle. `id` is stable across renames; bindings and
/// deep links point at it, never at the name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamProfile {
    pub id: String,
    /// User-facing; unique case-insensitively (menus are ambiguous otherwise).
    /// Editing UIs check [`ProfilesFile::name_taken`].
    pub name: String,
    /// Optional `#RRGGBB` chip. The schema reserves it; a UI may ignore it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    #[serde(default)]
    pub overrides: SettingsOverlay,
    /// Unknown keys a newer client wrote — preserved across a load→save round-trip.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl StreamProfile {
    /// Empty profile: inherits everything. Start from another via Duplicate.
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

/// Outcome of a `profile=` / `--profile` reference. Ambiguity refuses
/// rather than picking the first match (`design/client-deep-links.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    Found,
    NotFound,
    /// More than one profile carries this name (case-insensitively).
    Ambiguous,
}

/// Client-wide catalog. Per-host binding lives on the host record, not here.
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

    /// Stored catalog, or empty. A missing or unreadable file is "no profiles",
    /// never an error — streaming must not hinge on this file existing.
    pub fn load() -> ProfilesFile {
        Self::path()
            .map(|p| crate::trust::load_json_or_default(&p))
            .unwrap_or_default()
    }

    /// Persist temp+rename so a crash or full disk mid-write leaves the previous catalog intact.
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

    /// Exact id first, then a unique case-insensitive name. Ambiguous names
    /// are [`Resolution::Ambiguous`], never the first match.
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

    /// True if another profile already uses this name (case-insensitive).
    /// `except` is the profile being renamed, so "Work" → "work" is allowed.
    pub fn name_taken(&self, name: &str, except: Option<&str>) -> bool {
        self.profiles
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(name) && Some(p.id.as_str()) != except)
    }
}

/// 12 lowercase hex chars — the `library::new_id` shape, from the OS RNG.
pub fn new_profile_id() -> String {
    let b: [u8; 6] = rand::random();
    hex_lower(&b)
}

/// Random UUID-v4 in 8-4-4-4-12 form — host-record identity, matching
/// Apple's `StoredHost.id` so a deep-link host-ref is one format everywhere.
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
        assert!(
            out.gamepad_forwarding,
            "default on, and an empty overlay leaves it alone"
        );
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
            echo_cancel: Some(false),
            touch_mode: Some("pointer".into()),
            mouse_mode: Some("desktop".into()),
            invert_scroll: Some(true),
            inhibit_shortcuts: Some(false),
            gamepad: Some("dualsense".into()),
            gamepad_forwarding: Some(false),
            system_buttons: Some("local".into()),
            guide_gesture: Some("on".into()),
            match_window: Some(true),
            fullscreen_on_stream: Some(false),
            stats_verbosity: Some(StatsVerbosity::Detailed),
            present_priority: Some("smooth".into()),
            smooth_buffer: Some(3),
            vsync: Some(false),
            allow_vrr: Some(false),
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
        assert!(!out.echo_cancel);
        assert_eq!(out.touch_mode, "pointer");
        assert_eq!(out.mouse_mode, "desktop");
        assert!(out.invert_scroll);
        assert!(!out.inhibit_shortcuts);
        assert_eq!(out.gamepad, "dualsense");
        assert!(!out.gamepad_forwarding);
        assert_eq!(out.system_buttons, "local");
        assert_eq!(out.guide_gesture, "on");
        assert!(out.match_window);
        assert!(!out.fullscreen_on_stream);
        assert_eq!(out.stats_verbosity(), StatsVerbosity::Detailed);
        assert_eq!(out.present_priority, "smooth");
        assert_eq!(out.smooth_buffer, 3);
        assert!(!out.vsync);
        assert!(!out.allow_vrr);
        // Through the setter, so the legacy `show_stats` bool stays coherent.
        assert!(out.show_stats);
        // Tier-G/H fields are not in the overlay — decoder, endpoints, clipboard survive.
        assert_eq!(out.decoder, base.decoder);
        assert_eq!(out.speaker_device, base.speaker_device);

        // Equal to the base is still an override: a later global change must not move it.
        let pin = SettingsOverlay {
            bitrate_kbps: Some(20000),
            ..Default::default()
        };
        assert!(!pin.is_empty());
        let mut moved = base.clone();
        moved.bitrate_kbps = 50000;
        assert_eq!(pin.apply(&moved).bitrate_kbps, 20000);
    }

    #[test]
    fn absorb_records_the_touched_field_only() {
        let base = Settings {
            bitrate_kbps: 20000,
            codec: "hevc".into(),
            ..Default::default()
        };
        let mut o = SettingsOverlay::default();

        let before = o.apply(&base);
        let mut after = before.clone();
        after.codec = "av1".into();
        o.absorb(&before, &after);
        assert_eq!(o.codec.as_deref(), Some("av1"));
        assert_eq!(o.bitrate_kbps, None, "nothing else may be recorded");

        // Back to the global's value is still a pin — not a diff against globals at save.
        let before = o.apply(&base);
        let mut after = before.clone();
        after.codec = "hevc".into();
        o.absorb(&before, &after);
        assert_eq!(o.codec.as_deref(), Some("hevc"));
        let mut moved = base.clone();
        moved.codec = "h264".into();
        assert_eq!(o.apply(&moved).codec, "hevc");

        // Stats tier goes through the resolver, not the legacy bool.
        let before = o.apply(&base);
        let mut after = before.clone();
        after.set_stats_verbosity(StatsVerbosity::Detailed);
        o.absorb(&before, &after);
        assert_eq!(o.stats_verbosity, Some(StatsVerbosity::Detailed));

        let before = o.apply(&base);
        let mut o2 = o.clone();
        o2.absorb(&before, &before);
        assert_eq!(o2, o);
    }

    #[test]
    fn echo_cancel_is_a_first_class_override() {
        let base = Settings::default();
        assert!(base.echo_cancel, "the setting ships on");

        let mut o = SettingsOverlay::default();
        let before = o.apply(&base);
        let mut after = before.clone();
        after.echo_cancel = false;
        o.absorb(&before, &after);
        assert_eq!(o.echo_cancel, Some(false));
        assert!(!o.apply(&base).echo_cancel);
        assert!(
            o.extra.is_empty(),
            "modelled fields must never land in the passthrough"
        );

        let text = serde_json::to_string(&o).unwrap();
        assert!(text.contains("\"echo_cancel\":false"), "{text}");
        let from_apple: SettingsOverlay =
            serde_json::from_str(r#"{"mic_enabled":true,"echo_cancel":false}"#).unwrap();
        assert_eq!(from_apple.echo_cancel, Some(false));
        assert!(from_apple.extra.is_empty());

        assert!(o.clear("echo_cancel"));
        assert_eq!(o.echo_cancel, None);
        assert!(o.is_empty());
    }

    #[test]
    fn overlay_actions_is_a_first_class_override() {
        let base = Settings::default();
        assert!(
            base.overlay_actions.is_empty(),
            "empty = the platform default ring"
        );
        let blob = r#"{"v":2,"ring":["mic"]}"#;

        let mut o = SettingsOverlay::default();
        let before = o.apply(&base);
        let mut after = before.clone();
        after.overlay_actions = blob.into();
        o.absorb(&before, &after);
        assert_eq!(o.overlay_actions.as_deref(), Some(blob));
        assert_eq!(o.apply(&base).overlay_actions, blob);
        assert!(o.extra.is_empty());

        let text = serde_json::to_string(&o).unwrap();
        assert!(text.contains("\"overlay_actions\":"), "{text}");
        let from_apple: SettingsOverlay =
            serde_json::from_str(r#"{"overlay_actions":"{\"v\":2}"}"#).unwrap();
        assert_eq!(from_apple.overlay_actions.as_deref(), Some("{\"v\":2}"));
        assert!(from_apple.extra.is_empty());

        assert!(o.clear("overlay_actions"));
        assert!(o.is_empty());
    }

    /// Unmodelled keys survive load→save in `extra` without applying. This
    /// field must be first-class or a lossless profile would stream Opus.
    #[test]
    fn audio_format_is_a_first_class_override() {
        let base = Settings::default();
        assert_eq!(
            base.audio_format,
            crate::audio_format::AUDIO_FORMAT_OPUS,
            "the setting ships off"
        );

        let mut o = SettingsOverlay::default();
        let before = o.apply(&base);
        let mut after = before.clone();
        after.audio_format = crate::audio_format::AUDIO_FORMAT_LOSSLESS_96.into();
        o.absorb(&before, &after);
        assert_eq!(o.audio_format.as_deref(), Some("lossless96"));
        assert_eq!(o.apply(&base).audio_format, "lossless96");
        assert!(
            o.extra.is_empty(),
            "modelled fields must never land in the passthrough"
        );

        let text = serde_json::to_string(&o).unwrap();
        assert!(text.contains("\"audio_format\":\"lossless96\""), "{text}");
        let from_android: SettingsOverlay =
            serde_json::from_str(r#"{"audio_channels":2,"audio_format":"lossless48"}"#).unwrap();
        assert_eq!(from_android.audio_format.as_deref(), Some("lossless48"));
        assert!(from_android.extra.is_empty());
        assert_eq!(from_android.apply(&base).audio_format, "lossless48");

        assert!(o.clear("audio_format"));
        assert_eq!(o.audio_format, None);
        assert!(o.is_empty());
        // Inherit the global — not a remembered "lossless96".
        assert_eq!(
            o.apply(&base).audio_format,
            crate::audio_format::AUDIO_FORMAT_OPUS
        );
    }

    #[test]
    fn presentation_cluster_is_first_class() {
        let base = Settings::default();
        let mut o = SettingsOverlay::default();
        let before = o.apply(&base);
        let mut after = before.clone();
        after.present_priority = "smooth".into();
        o.absorb(&before, &after);
        let before = o.apply(&base);
        let mut after = before.clone();
        after.smooth_buffer = 1;
        o.absorb(&before, &after);
        assert_eq!(o.present_priority.as_deref(), Some("smooth"));
        assert_eq!(o.smooth_buffer, Some(1));
        assert!(
            o.extra.is_empty(),
            "modelled fields must never land in the passthrough"
        );
        let out = o.apply(&base);
        assert_eq!(
            out.present_priority(),
            crate::trust::PresentPriority::Smooth { buffer: 1 }
        );

        let text = serde_json::to_string(&o).unwrap();
        assert!(text.contains("\"present_priority\":\"smooth\""), "{text}");
        assert!(text.contains("\"smooth_buffer\":1"), "{text}");
        let from_apple: SettingsOverlay = serde_json::from_str(
            r#"{"present_priority":"latency","smooth_buffer":2,"vsync":true,"allow_vrr":false}"#,
        )
        .unwrap();
        assert_eq!(from_apple.present_priority.as_deref(), Some("latency"));
        assert_eq!(from_apple.smooth_buffer, Some(2));
        assert_eq!(from_apple.vsync, Some(true));
        assert_eq!(from_apple.allow_vrr, Some(false));
        assert!(from_apple.extra.is_empty());

        assert!(o.clear("present_priority"));
        assert!(o.clear("smooth_buffer"));
        assert_eq!(o.present_priority, None);
        assert!(o.is_empty());
        let mut vrr = from_apple;
        assert!(vrr.clear("vsync"));
        assert!(vrr.clear("allow_vrr"));
        assert_eq!((vrr.vsync, vrr.allow_vrr), (None, None));
    }

    #[test]
    fn clear_drops_one_override() {
        let mut o = SettingsOverlay {
            width: Some(3840),
            height: Some(2160),
            match_window: Some(false),
            codec: Some("av1".into()),
            ..Default::default()
        };
        assert!(o.clear("codec"));
        assert_eq!(o.codec, None);
        assert!(o.clear("resolution"));
        assert_eq!((o.width, o.height, o.match_window), (None, None, None));
        assert!(o.is_empty());
        assert!(!o.clear("no_such_field"));
    }

    /// `false` is the interesting override (default is on). Dropping it in
    /// `apply` would silently forward a pad the profile refused.
    #[test]
    fn gamepad_forwarding_overrides_off_and_resets_back() {
        let base = Settings::default();
        assert!(base.gamepad_forwarding, "the shipped default");

        let mut o = SettingsOverlay::default();
        let mut after = base.clone();
        after.gamepad_forwarding = false;
        o.absorb(&base, &after);
        assert_eq!(o.gamepad_forwarding, Some(false));
        assert!(!o.apply(&base).gamepad_forwarding);

        assert!(o.clear("gamepad_forwarding"));
        assert_eq!(o.gamepad_forwarding, None);
        assert!(o.is_empty());
        // Inherit the live global, not a remembered false.
        assert!(o.apply(&base).gamepad_forwarding);
    }

    /// Off is a legitimate override. The setter keeps `show_stats` in sync.
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

    /// Unknown codec strings stay as written; unknown overlay keys survive
    /// load→save rather than being erased.
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
        // Missing `overrides` key = inherit everything.
        assert!(file
            .find_by_id("0f0f0f0f0f0f")
            .unwrap()
            .overrides
            .is_empty());

        let text = serde_json::to_string(&file).unwrap();
        assert!(text.contains("vvc-from-the-future"));
        assert!(text.contains("some_new_axis"));
        assert!(text.contains("future_profile_key"));
        // Absent overrides omit rather than serialize as null.
        assert!(!text.contains("null"));
        let round: ProfilesFile = serde_json::from_str(&text).unwrap();
        let game = round.find_by_id("a1b2c3d4e5f6").unwrap();
        assert_eq!(game.overrides.width, Some(3840));
        assert_eq!(game.overrides.extra.len(), 1);
        assert_eq!(game.extra.len(), 1);

        // Unknown codec still applies: the host, not this overlay, decides what it can encode.
        let applied = game.overrides.apply(&Settings::default());
        assert_eq!(applied.codec, "vvc-from-the-future");
    }

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

        assert!(file.name_taken("GAME", None));
        assert!(!file.name_taken("GAME", Some("333333333333")));
        assert!(file.name_taken("GAME", Some("111111111111")));
        assert!(!file.name_taken("Travel", None));
    }

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
