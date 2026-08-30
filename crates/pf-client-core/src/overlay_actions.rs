//! The in-stream quick-action ring's configuration: the `overlay_actions` setting, one JSON
//! blob (schema `v: 2`, design/touch-client-overlay.md §3.2). Six ring slots clockwise from 12
//! o'clock, the custom shortcuts, and the virtual pad's preset.
//!
//! Contract: [`OverlayConfig::parse`] never fails. Fewer than six slots pad with empty, more are
//! truncated, an unknown slot id or a dangling `shortcut:` reference is an empty slot, an absent
//! field takes its default, and an unparseable blob is the platform default. Profiles sync
//! between client versions, so a newer client's ring must degrade quietly on an older one.
//!
//! The Swift (`OverlayActions.swift`) and Kotlin (`OverlayActions.kt`) ports mirror this file;
//! the tests here are the contract they port.

use serde::{Deserialize, Serialize};

/// Slots on the ring, clockwise from 12 o'clock.
pub const RING_SLOTS: usize = 6;

/// What a ring slot does. `Host` carries a host-advertised action id (`power.sleep`);
/// `Shortcut` refers into [`OverlayConfig::shortcuts`] by id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlotId {
    EndStream,
    DisconnectLinger,
    TouchMode,
    Keyboard,
    Stats,
    Mic,
    Pad,
    SendText,
    Host(String),
    Shortcut(String),
}

impl SlotId {
    /// The wire id back, the inverse of [`SlotId::parse`].
    pub fn id(&self) -> String {
        match self {
            SlotId::EndStream => "end_stream".into(),
            SlotId::DisconnectLinger => "disconnect_linger".into(),
            SlotId::TouchMode => "touch_mode".into(),
            SlotId::Keyboard => "keyboard".into(),
            SlotId::Stats => "stats".into(),
            SlotId::Mic => "mic".into(),
            SlotId::Pad => "pad".into(),
            SlotId::SendText => "send_text".into(),
            SlotId::Host(id) => format!("host:{id}"),
            SlotId::Shortcut(id) => format!("shortcut:{id}"),
        }
    }

    /// An id from the blob; `None` for one this build does not know (an empty slot).
    pub fn parse(s: &str) -> Option<SlotId> {
        Some(match s {
            "end_stream" => SlotId::EndStream,
            "disconnect_linger" => SlotId::DisconnectLinger,
            "touch_mode" => SlotId::TouchMode,
            "keyboard" => SlotId::Keyboard,
            "stats" => SlotId::Stats,
            "mic" => SlotId::Mic,
            "pad" => SlotId::Pad,
            "send_text" => SlotId::SendText,
            _ => {
                if let Some(id) = s.strip_prefix("host:").filter(|id| !id.is_empty()) {
                    SlotId::Host(id.into())
                } else if let Some(id) = s.strip_prefix("shortcut:").filter(|id| !id.is_empty()) {
                    SlotId::Shortcut(id.into())
                } else {
                    return None;
                }
            }
        })
    }
}

/// A custom key chord. `keys` are names from the shared keymap tables (`ctrl`, `shift`,
/// `escape`, `tab`, `f4`, `a`…), never raw virtual-key codes, so one profile works on every
/// client.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shortcut {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub keys: Vec<String>,
}

/// The Windows virtual-key code a shortcut key name stands for — the wire speaks VKs, and a
/// profile written on one client must fire on every other, so names, not codes, are stored.
/// Modifiers, navigation keys, `f1`…`f24`, `a`…`z`, `0`…`9`; `None` for a name this build does
/// not know (the chord then does not fire, and the editor shows the key as unknown).
pub fn key_vk(name: &str) -> Option<u8> {
    let n = name.trim().to_ascii_lowercase();
    let vk = match n.as_str() {
        "ctrl" | "control" => 0x11,
        "shift" => 0x10,
        "alt" | "option" => 0x12,
        "win" | "cmd" | "super" | "meta" => 0x5B,
        "escape" | "esc" => 0x1B,
        "tab" => 0x09,
        "enter" | "return" => 0x0D,
        "space" => 0x20,
        "backspace" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "printscreen" => 0x2C,
        "pause" => 0x13,
        "capslock" => 0x14,
        _ => {
            let b = n.as_bytes();
            return match b {
                [c @ b'a'..=b'z'] => Some(0x41 + (c - b'a')),
                [c @ b'0'..=b'9'] => Some(0x30 + (c - b'0')),
                [b'f', rest @ ..] if !rest.is_empty() => n[1..]
                    .parse::<u8>()
                    .ok()
                    .filter(|f| (1..=24).contains(f))
                    .map(|f| 0x70 + f - 1),
                _ => None,
            };
        }
    };
    Some(vk)
}

/// A chord as a legend reads it: `Ctrl+Shift+Esc`.
pub fn chord_chip(keys: &[String]) -> String {
    keys.iter()
        .map(|k| key_legend(k))
        .collect::<Vec<_>>()
        .join("+")
}

/// One key's legend: the word a keyboard prints on it (`Ctrl`, `Esc`, `PgUp`), arrows as
/// arrows. Symbols like ❖ or ⇧ read as nothing to most people, so none are used here.
pub fn key_legend(k: &str) -> String {
    match k.trim().to_ascii_lowercase().as_str() {
        "ctrl" | "control" => "Ctrl".to_string(),
        "shift" => "Shift".into(),
        "alt" | "option" => "Alt".into(),
        "win" | "cmd" | "super" | "meta" => "Win".into(),
        "escape" | "esc" => "Esc".into(),
        "enter" | "return" => "Enter".into(),
        "backspace" => "Backspace".into(),
        "delete" | "del" => "Del".into(),
        "insert" => "Ins".into(),
        "pageup" => "PgUp".into(),
        "pagedown" => "PgDn".into(),
        "printscreen" => "PrtSc".into(),
        "capslock" => "Caps".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        other => {
            let mut c = other.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        }
    }
}

/// The virtual controller's preset (Android and Apple only): `layout` is `full`, `sticks` or
/// `dpad`; `opacity` and `scale` are the two sliders.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PadConfig {
    pub layout: String,
    pub opacity: f32,
    pub scale: f32,
}

impl Default for PadConfig {
    fn default() -> Self {
        PadConfig {
            layout: "full".into(),
            opacity: 0.45,
            scale: 1.0,
        }
    }
}

/// Which platform default ring applies: phones and tablets carry the keyboard and the virtual
/// pad; desktops carry the linger disconnect and send-text instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingPlatform {
    Touch,
    Desktop,
}

/// The parsed setting.
#[derive(Clone, Debug, PartialEq)]
pub struct OverlayConfig {
    pub ring: [Option<SlotId>; RING_SLOTS],
    pub shortcuts: Vec<Shortcut>,
    pub pad: PadConfig,
}

/// The blob's shape on disk. Lenient by construction: every field defaults.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Raw {
    v: u32,
    ring: Vec<Option<String>>,
    shortcuts: Vec<Shortcut>,
    pad: PadConfig,
}

impl OverlayConfig {
    pub const SCHEMA_VERSION: u32 = 2;

    pub fn platform_default(platform: RingPlatform) -> Self {
        let ring = match platform {
            RingPlatform::Touch => [
                Some(SlotId::EndStream),
                Some(SlotId::Keyboard),
                Some(SlotId::TouchMode),
                Some(SlotId::Stats),
                Some(SlotId::Mic),
                Some(SlotId::Pad),
            ],
            RingPlatform::Desktop => [
                Some(SlotId::EndStream),
                Some(SlotId::DisconnectLinger),
                Some(SlotId::TouchMode),
                Some(SlotId::Stats),
                Some(SlotId::Mic),
                Some(SlotId::SendText),
            ],
        };
        OverlayConfig {
            ring,
            shortcuts: Vec::new(),
            pad: PadConfig::default(),
        }
    }

    /// Parse the setting. An empty or unparseable blob is the platform default; everything
    /// else degrades slot by slot (module docs).
    pub fn parse(json: &str, platform: RingPlatform) -> Self {
        if json.trim().is_empty() {
            return Self::platform_default(platform);
        }
        let raw: Raw = match serde_json::from_str(json) {
            Ok(r) => r,
            Err(_) => return Self::platform_default(platform),
        };
        let shortcuts: Vec<Shortcut> = raw
            .shortcuts
            .into_iter()
            .filter(|s| !s.id.is_empty())
            .collect();
        let mut ring: [Option<SlotId>; RING_SLOTS] = Default::default();
        for (slot, id) in ring.iter_mut().zip(raw.ring) {
            *slot = id.as_deref().and_then(SlotId::parse).filter(|s| match s {
                SlotId::Shortcut(id) => shortcuts.iter().any(|sc| &sc.id == id),
                _ => true,
            });
        }
        OverlayConfig {
            ring,
            shortcuts,
            pad: raw.pad,
        }
    }

    /// The blob to store — always the current schema version.
    pub fn to_json(&self) -> String {
        let raw = Raw {
            v: Self::SCHEMA_VERSION,
            ring: self
                .ring
                .iter()
                .map(|s| s.as_ref().map(SlotId::id))
                .collect(),
            shortcuts: self.shortcuts.clone(),
            pad: self.pad.clone(),
        };
        serde_json::to_string(&raw).expect("plain data serializes")
    }

    /// The shortcut a `shortcut:<id>` slot refers to.
    pub fn shortcut(&self, id: &str) -> Option<&Shortcut> {
        self.shortcuts.iter().find(|s| s.id == id)
    }

    /// Write a shortcut: over its own entry when `id` names one, else appended with the next
    /// `s<n>` id and into the first empty slot (design §3.3). Returns the id it went under.
    /// One implementation for every editor, so a shortcut made on any client lands the same.
    pub fn upsert_shortcut(&mut self, id: Option<&str>, label: &str, keys: Vec<String>) -> String {
        let label = label.trim().to_string();
        if let Some(sc) = id.and_then(|id| self.shortcuts.iter_mut().find(|s| s.id == id)) {
            sc.label = label;
            sc.keys = keys;
            return sc.id.clone();
        }
        let next = self
            .shortcuts
            .iter()
            .filter_map(|s| s.id.trim_start_matches('s').parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        let id = format!("s{next}");
        if let Some(slot) = self.ring.iter_mut().find(|s| s.is_none()) {
            *slot = Some(SlotId::Shortcut(id.clone()));
        }
        self.shortcuts.push(Shortcut {
            id: id.clone(),
            label,
            keys,
        });
        id
    }

    /// Drop a shortcut and empty the slot that pointed at it (`parse` would on the next read;
    /// doing it here shows it at once).
    pub fn remove_shortcut(&mut self, id: &str) {
        self.shortcuts.retain(|s| s.id != id);
        for slot in self.ring.iter_mut() {
            if matches!(slot, Some(SlotId::Shortcut(s)) if s == id) {
                *slot = None;
            }
        }
    }
}

/// One thing a slot can hold, as an editor lists it: the wire id (empty for the empty slot),
/// the label, and the note that says where it is unavailable (empty when it is not).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogueEntry {
    pub id: String,
    pub label: String,
    pub note: String,
}

/// A group of the catalogue, in the order every editor shows them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogueGroup {
    pub title: &'static str,
    pub entries: Vec<CatalogueEntry>,
}

/// The catalogue by group (design §3.3): Session, Input, View, Audio, Host, the blob's own
/// Shortcuts (when there are any), Empty last. One table for every editor — the shells render
/// it, they do not restate it. The notes are the platform's: a desktop has no virtual
/// controller and no typed-text path; a phone has both.
pub fn catalogue(cfg: &OverlayConfig, platform: RingPlatform) -> Vec<CatalogueGroup> {
    let e = |id: &str, label: &str, note: &str| CatalogueEntry {
        id: id.into(),
        label: label.into(),
        note: note.into(),
    };
    let desktop = platform == RingPlatform::Desktop;
    let host = "Only where the host offers it";
    let mut g = vec![
        CatalogueGroup {
            title: "Session",
            entries: vec![
                e("end_stream", "End stream", ""),
                e("disconnect_linger", "Disconnect, keep the game running", ""),
            ],
        },
        CatalogueGroup {
            title: "Input",
            entries: vec![
                e("touch_mode", "Touch mode", ""),
                e("keyboard", "Keyboard", ""),
                e(
                    "pad",
                    "Virtual controller",
                    if desktop {
                        "Phones and tablets only"
                    } else {
                        ""
                    },
                ),
                e(
                    "send_text",
                    "Send text",
                    if desktop {
                        "Not on this client yet"
                    } else {
                        ""
                    },
                ),
            ],
        },
        CatalogueGroup {
            title: "View",
            entries: vec![e("stats", "Statistics", "")],
        },
        CatalogueGroup {
            title: "Audio",
            entries: vec![e("mic", "Microphone", "")],
        },
        CatalogueGroup {
            title: "Host",
            entries: vec![
                e("host:power.sleep", "Sleep host", host),
                e("host:power.reboot", "Restart host", host),
                e("host:power.shutdown", "Shut down host", host),
            ],
        },
    ];
    if !cfg.shortcuts.is_empty() {
        g.push(CatalogueGroup {
            title: "Shortcuts",
            entries: cfg
                .shortcuts
                .iter()
                .map(|sc| {
                    let chip = chord_chip(&sc.keys);
                    if sc.label.is_empty() {
                        e(&format!("shortcut:{}", sc.id), &chip, "")
                    } else {
                        e(&format!("shortcut:{}", sc.id), &sc.label, &chip)
                    }
                })
                .collect(),
        });
    }
    g.push(CatalogueGroup {
        title: "Empty",
        entries: vec![e("", "Empty slot", "")],
    });
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalogue is one list in one order on every editor: the groups, the desktop notes
    /// a phone does not carry, the blob's shortcuts by name with the legend as the note, and
    /// Empty last — with no Shortcuts group when there are none.
    #[test]
    fn the_catalogue_is_grouped_noted_per_platform_and_ends_with_empty() {
        let blob = r#"{"v":2,"ring":[],"shortcuts":[{"id":"s1","label":"Task Manager","keys":["ctrl","shift","escape"]},{"id":"s2","keys":["alt","f4"]}]}"#;
        let cfg = OverlayConfig::parse(blob, RingPlatform::Desktop);
        let groups = catalogue(&cfg, RingPlatform::Desktop);
        let titles: Vec<&str> = groups.iter().map(|g| g.title).collect();
        assert_eq!(
            titles,
            [
                "Session",
                "Input",
                "View",
                "Audio",
                "Host",
                "Shortcuts",
                "Empty"
            ]
        );
        let pad = &groups[1].entries[2];
        assert_eq!(
            (pad.id.as_str(), pad.note.as_str()),
            ("pad", "Phones and tablets only")
        );
        let phone = catalogue(&cfg, RingPlatform::Touch);
        assert_eq!(phone[1].entries[2].note, "", "a phone has the pad");
        let s = &groups[5].entries;
        assert_eq!(
            (s[0].id.as_str(), s[0].label.as_str(), s[0].note.as_str()),
            ("shortcut:s1", "Task Manager", "Ctrl+Shift+Esc")
        );
        assert_eq!(
            (s[1].id.as_str(), s[1].label.as_str(), s[1].note.as_str()),
            ("shortcut:s2", "Alt+F4", "")
        );
        assert_eq!(groups[6].entries[0].id, "");
        let none = catalogue(
            &OverlayConfig::parse("", RingPlatform::Desktop),
            RingPlatform::Desktop,
        );
        assert!(none.iter().all(|g| g.title != "Shortcuts"));
    }

    /// A new shortcut takes the next id and the first empty slot; written again under its id
    /// it changes in place; removed, its slot empties.
    #[test]
    fn a_shortcut_is_upserted_by_id_and_takes_the_first_empty_slot() {
        let mut cfg = OverlayConfig::parse(
            r#"{"v":2,"ring":["end_stream",null,null,null,null,null]}"#,
            RingPlatform::Desktop,
        );
        let id = cfg.upsert_shortcut(
            None,
            " Task Manager ",
            vec!["ctrl".into(), "shift".into(), "escape".into()],
        );
        assert_eq!(id, "s1");
        assert_eq!(cfg.ring[1], Some(SlotId::Shortcut("s1".into())));
        assert_eq!(cfg.shortcuts[0].label, "Task Manager");
        let again = cfg.upsert_shortcut(Some("s1"), "Tasks", vec!["ctrl".into(), "escape".into()]);
        assert_eq!(again, "s1");
        assert_eq!(cfg.shortcuts.len(), 1);
        assert_eq!(cfg.shortcuts[0].keys, vec!["ctrl", "escape"]);
        let second = cfg.upsert_shortcut(None, "", vec!["alt".into(), "f4".into()]);
        assert_eq!(second, "s2");
        assert_eq!(cfg.ring[2], Some(SlotId::Shortcut("s2".into())));
        cfg.remove_shortcut("s1");
        assert_eq!(cfg.shortcuts.len(), 1);
        assert_eq!(cfg.ring[1], None);
        assert_eq!(cfg.ring[2], Some(SlotId::Shortcut("s2".into())));
    }

    const FULL: &str = r#"{"v":2,
        "ring":["end_stream","shortcut:s1","host:power.sleep","stats",null,"pad"],
        "shortcuts":[{"id":"s1","label":"Task Manager","keys":["ctrl","shift","escape"]}],
        "pad":{"layout":"sticks","opacity":0.3,"scale":1.2}}"#;

    #[test]
    fn round_trips_through_json() {
        let cfg = OverlayConfig::parse(FULL, RingPlatform::Touch);
        assert_eq!(cfg.ring[1], Some(SlotId::Shortcut("s1".into())));
        assert_eq!(cfg.ring[2], Some(SlotId::Host("power.sleep".into())));
        assert_eq!(cfg.ring[4], None);
        assert_eq!(cfg.pad.layout, "sticks");
        assert_eq!(
            cfg.shortcut("s1").unwrap().keys,
            ["ctrl", "shift", "escape"]
        );
        let again = OverlayConfig::parse(&cfg.to_json(), RingPlatform::Touch);
        assert_eq!(again, cfg);
    }

    #[test]
    fn short_rings_pad_and_long_rings_truncate() {
        let short = OverlayConfig::parse(r#"{"ring":["mic"]}"#, RingPlatform::Desktop);
        assert_eq!(short.ring[0], Some(SlotId::Mic));
        assert!(short.ring[1..].iter().all(Option::is_none));
        let long = OverlayConfig::parse(
            r#"{"ring":["mic","mic","mic","mic","mic","mic","stats","stats"]}"#,
            RingPlatform::Desktop,
        );
        assert!(long.ring.iter().all(|s| *s == Some(SlotId::Mic)));
    }

    #[test]
    fn unknown_ids_and_dangling_shortcuts_are_empty_slots() {
        let cfg = OverlayConfig::parse(
            r#"{"ring":["teleport","shortcut:nope","host:","stats"]}"#,
            RingPlatform::Touch,
        );
        assert_eq!(cfg.ring[0], None, "a newer client's id degrades to empty");
        assert_eq!(cfg.ring[1], None, "no such shortcut");
        assert_eq!(cfg.ring[2], None, "a host id needs a name");
        assert_eq!(cfg.ring[3], Some(SlotId::Stats));
    }

    #[test]
    fn empty_or_broken_blobs_are_the_platform_default() {
        let touch = OverlayConfig::platform_default(RingPlatform::Touch);
        let desktop = OverlayConfig::platform_default(RingPlatform::Desktop);
        assert_eq!(OverlayConfig::parse("", RingPlatform::Touch), touch);
        assert_eq!(
            OverlayConfig::parse("{not json", RingPlatform::Desktop),
            desktop
        );
        assert_eq!(touch.ring[5], Some(SlotId::Pad));
        assert_eq!(desktop.ring[5], Some(SlotId::SendText));
        // Absent fields take their defaults; a present ring does not disturb the pad.
        let cfg = OverlayConfig::parse(r#"{"v":2,"ring":[]}"#, RingPlatform::Touch);
        assert_eq!(cfg.pad, PadConfig::default());
        assert!(cfg.ring.iter().all(Option::is_none));
    }

    #[test]
    fn key_names_map_to_windows_vks() {
        assert_eq!(key_vk("ctrl"), Some(0x11));
        assert_eq!(key_vk("Shift"), Some(0x10));
        assert_eq!(key_vk("escape"), Some(0x1B));
        assert_eq!(key_vk("tab"), Some(0x09));
        assert_eq!(key_vk("a"), Some(0x41));
        assert_eq!(key_vk("z"), Some(0x5A));
        assert_eq!(key_vk("0"), Some(0x30));
        assert_eq!(key_vk("f1"), Some(0x70));
        assert_eq!(key_vk("f12"), Some(0x7B));
        assert_eq!(key_vk("f25"), None);
        assert_eq!(key_vk("hyper"), None);
        assert_eq!(key_vk(""), None);
        let keys: Vec<String> = ["ctrl", "shift", "escape"].map(String::from).into();
        assert_eq!(chord_chip(&keys), "Ctrl+Shift+Esc");
        assert_eq!(key_legend("win"), "Win");
        assert_eq!(key_legend("pageup"), "PgUp");
        assert_eq!(key_legend("f4"), "F4");
        assert_eq!(key_legend("left"), "←");
    }

    #[test]
    fn slot_ids_are_stable_strings() {
        for id in [
            "end_stream",
            "disconnect_linger",
            "touch_mode",
            "keyboard",
            "stats",
            "mic",
            "pad",
            "send_text",
            "host:power.reboot",
            "shortcut:s2",
        ] {
            assert_eq!(SlotId::parse(id).unwrap().id(), id);
        }
    }
}
