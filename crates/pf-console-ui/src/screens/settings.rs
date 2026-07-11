//! The console settings screen — the couch-relevant subset of the Settings store,
//! restyled as glass rows and fully controller-navigable (the Swift
//! `GamepadSettingsView`, re-homed): up/down moves focus, left/right steps the focused
//! value (clamped — the boundary thud tells the thumb it's the last option), A cycles
//! forward wrapping, B closes. Every change persists immediately; the desktop shells
//! read the same file, so values round-trip freely.

use crate::glyphs::{Hint, HintKey};
use crate::screens::{Ctx, Outbox};
use crate::theme::{Fonts, DIM, W};
use crate::widgets::{ListMsg, MenuList, RowSpec};
use pf_client_core::gamepad::{MenuEvent, MenuPulse};
use pf_client_core::trust::StatsVerbosity;
use skia_safe::{Canvas, Rect};

/// Stable row identity — adjust/activate dispatch by id so nothing acts on a stale
/// index when the pad list under the "Use controller" row churns.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowId {
    Resolution,
    Refresh,
    Bitrate,
    Compositor,
    Codec,
    Decoder,
    Hdr,
    Audio,
    Mic,
    Pad,
    PadType,
    Stats,
}

const ROWS: [RowId; 12] = [
    RowId::Resolution,
    RowId::Refresh,
    RowId::Bitrate,
    RowId::Compositor,
    RowId::Codec,
    RowId::Decoder,
    RowId::Hdr,
    RowId::Audio,
    RowId::Mic,
    RowId::Pad,
    RowId::PadType,
    RowId::Stats,
];

const RESOLUTIONS: [(u32, u32); 6] = [
    (0, 0), // native
    (1280, 720),
    (1280, 800), // the Deck's panel
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];
const REFRESH: [u32; 5] = [0, 30, 60, 90, 120];
const BITRATES: [u32; 7] = [0, 5_000, 10_000, 20_000, 30_000, 50_000, 80_000];
const COMPOSITORS: [(&str, &str); 5] = [
    ("auto", "Automatic"),
    ("kwin", "KWin"),
    ("wlroots", "wlroots"),
    ("mutter", "Mutter"),
    ("gamescope", "gamescope"),
];
const CODECS: [(&str, &str); 4] = [
    ("auto", "Automatic"),
    ("hevc", "HEVC"),
    ("h264", "H.264"),
    ("av1", "AV1"),
];
const DECODERS: [(&str, &str); 4] = [
    ("auto", "Automatic"),
    ("vulkan", "Vulkan Video"),
    ("vaapi", "VAAPI"),
    ("software", "Software"),
];
const AUDIO: [(u8, &str); 3] = [(2, "Stereo"), (6, "5.1"), (8, "7.1")];
const PAD_TYPES: [(&str, &str); 6] = [
    ("auto", "Automatic"),
    ("xbox360", "Xbox 360"),
    ("xboxone", "Xbox One"),
    ("dualsense", "DualSense"),
    ("dualshock4", "DualShock 4"),
    ("steamdeck", "Steam Deck"),
];

pub(crate) struct SettingsScreen {
    list: MenuList,
}

impl SettingsScreen {
    pub(crate) fn new() -> SettingsScreen {
        SettingsScreen {
            list: MenuList::new(),
        }
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        let (msg, pulse) = self.list.menu(ev, ROWS.len());
        match msg {
            ListMsg::Adjust(delta) => {
                let changed = adjust(ROWS[self.list.cursor], delta, false, ctx);
                if changed {
                    ctx.settings.save();
                    Some(MenuPulse::Move)
                } else {
                    Some(MenuPulse::Boundary)
                }
            }
            ListMsg::Activate => {
                // A cycles forward WRAPPING, so every option is reachable one-handed.
                if adjust(ROWS[self.list.cursor], 1, true, ctx) {
                    ctx.settings.save();
                }
                pulse
            }
            ListMsg::None => pulse,
        }
    }

    pub(crate) fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        vec![
            Hint::new(HintKey::Adjust, "Adjust"),
            Hint::new(HintKey::Confirm, "Change"),
            Hint::new(HintKey::Back, "Done"),
        ]
    }

    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        // The focused row's explainer sits in a reserved band under the list.
        let detail_h = 34.0 * k;
        let list_rect = Rect::from_ltrb(
            rect.left,
            rect.top,
            rect.right,
            rect.bottom - detail_h as f32,
        );
        let rows: Vec<RowSpec> = ROWS.iter().map(|id| row_spec(*id, ctx)).collect();
        self.list
            .render(canvas, list_rect, &rows, fonts, k, dt, true);
        let detail = detail(ROWS[self.list.cursor]);
        fonts.centered(
            canvas,
            detail,
            W::Regular,
            13.0 * k,
            DIM,
            f64::from(rect.left) + f64::from(rect.width()) / 2.0,
            f64::from(rect.bottom) - detail_h + 6.0 * k,
            f64::from(rect.width()) * 0.8,
        );
    }
}

fn row_spec(id: RowId, ctx: &Ctx) -> RowSpec {
    let s = &ctx.settings;
    let (header, label, value): (Option<&'static str>, &str, String) = match id {
        RowId::Resolution => (
            Some("Stream"),
            "Resolution",
            if s.match_window {
                "Match window".into()
            } else if s.width == 0 {
                "Native".into()
            } else {
                format!("{} × {}", s.width, s.height)
            },
        ),
        RowId::Refresh => (
            None,
            "Refresh rate",
            if s.refresh_hz == 0 {
                "Native".into()
            } else {
                format!("{} Hz", s.refresh_hz)
            },
        ),
        RowId::Bitrate => (
            None,
            "Bitrate",
            if s.bitrate_kbps == 0 {
                "Automatic".into()
            } else {
                format!("{} Mbps", s.bitrate_kbps / 1000)
            },
        ),
        RowId::Compositor => (
            None,
            "Compositor",
            label_for(&COMPOSITORS, &s.compositor).into(),
        ),
        RowId::Codec => (
            Some("Video"),
            "Video codec",
            label_for(&CODECS, &s.codec).into(),
        ),
        RowId::Decoder => (None, "Decoder", label_for(&DECODERS, &s.decoder).into()),
        RowId::Hdr => (None, "10-bit HDR", on_off(s.hdr_enabled).into()),
        RowId::Audio => (
            Some("Audio"),
            "Audio channels",
            AUDIO
                .iter()
                .find(|(v, _)| *v == s.audio_channels)
                .map_or("Stereo", |(_, l)| l)
                .into(),
        ),
        RowId::Mic => (None, "Microphone", on_off(s.mic_enabled).into()),
        RowId::Pad => (
            Some("Controller"),
            "Use controller",
            if s.forward_pad.is_empty() {
                "Automatic".into()
            } else {
                ctx.pads
                    .iter()
                    .find(|p| p.key == s.forward_pad)
                    .map_or_else(|| "Saved (disconnected)".to_string(), |p| p.name.clone())
            },
        ),
        RowId::PadType => (
            None,
            "Controller type",
            label_for(&PAD_TYPES, &s.gamepad).into(),
        ),
        RowId::Stats => (
            Some("Interface"),
            "Statistics overlay",
            s.stats_verbosity().label().into(),
        ),
    };
    RowSpec {
        header,
        label: label.into(),
        value: Some(value),
        value_dim: false,
        caret: false,
        adjustable: true,
        enabled: true,
    }
}

fn detail(id: RowId) -> &'static str {
    match id {
        RowId::Resolution => {
            "The host creates a virtual display at exactly this size — no scaling. \
             Match window follows this window, including mid-stream resizes."
        }
        RowId::Refresh => "Native follows the display this window is on.",
        RowId::Bitrate => "Automatic uses the host's default (20 Mbps).",
        RowId::Compositor => {
            "Which compositor drives the virtual output — honored only if available on the host."
        }
        RowId::Codec => "A preference — the host falls back if it can't encode this one.",
        RowId::Decoder => "Automatic prefers Vulkan Video, then VAAPI, then software.",
        RowId::Hdr => {
            "HDR10 — engages when the host sends HDR content and this display supports it."
        }
        RowId::Audio => "The speaker layout requested from the host.",
        RowId::Mic => "Send this device's microphone to the host's virtual mic.",
        RowId::Pad => "Which pad is forwarded to the host, as player 1.",
        RowId::PadType => "The virtual pad the host creates — Automatic matches this controller.",
        RowId::Stats => {
            "How much the overlay shows: Compact (one line) → Normal → Detailed. \
             Ctrl+Alt+Shift+S cycles it live while streaming."
        }
    }
}

fn on_off(v: bool) -> &'static str {
    if v {
        "On"
    } else {
        "Off"
    }
}

fn label_for<'a>(options: &'a [(&str, &'a str)], value: &str) -> &'a str {
    options
        .iter()
        .find(|(v, _)| *v == value)
        .map_or("—", |(_, l)| l)
}

/// Step (`wrap=false`, clamped — false = boundary) or cycle (`wrap=true`) a row's
/// value. Toggles read left = off, right = on; a no-op is a boundary.
fn adjust(id: RowId, delta: i32, wrap: bool, ctx: &mut Ctx) -> bool {
    let s = &mut *ctx.settings;
    match id {
        RowId::Resolution => {
            // The D1 tri-state as one picker: Native, Match window, then the explicit
            // sizes (RESOLUTIONS keeps its (0,0) = Native head; Match window is the
            // virtual index 1, stored as the `match_window` flag with w/h cleared).
            let cur = if s.match_window {
                Some(1)
            } else {
                RESOLUTIONS
                    .iter()
                    .position(|(w, h)| (*w, *h) == (s.width, s.height))
                    .map(|i| if i == 0 { 0 } else { i + 1 })
            };
            step_option(cur, RESOLUTIONS.len() + 1, delta, wrap).map(|i| {
                s.match_window = i == 1;
                (s.width, s.height) = if i <= 1 { (0, 0) } else { RESOLUTIONS[i - 1] };
            })
        }
        RowId::Refresh => {
            let cur = REFRESH.iter().position(|r| *r == s.refresh_hz);
            step_option(cur, REFRESH.len(), delta, wrap).map(|i| s.refresh_hz = REFRESH[i])
        }
        RowId::Bitrate => {
            let cur = BITRATES.iter().position(|b| *b == s.bitrate_kbps);
            step_option(cur, BITRATES.len(), delta, wrap).map(|i| s.bitrate_kbps = BITRATES[i])
        }
        RowId::Compositor => step_str(&COMPOSITORS, &mut s.compositor, delta, wrap),
        RowId::Codec => step_str(&CODECS, &mut s.codec, delta, wrap),
        RowId::Decoder => step_str(&DECODERS, &mut s.decoder, delta, wrap),
        RowId::Hdr => toggle(&mut s.hdr_enabled, delta, wrap),
        RowId::Audio => {
            let cur = AUDIO.iter().position(|(v, _)| *v == s.audio_channels);
            step_option(cur, AUDIO.len(), delta, wrap).map(|i| s.audio_channels = AUDIO[i].0)
        }
        RowId::Mic => toggle(&mut s.mic_enabled, delta, wrap),
        RowId::Pad => {
            // Automatic first, then every connected pad by stable key.
            let keys: Vec<String> = std::iter::once(String::new())
                .chain(ctx.pads.iter().map(|p| p.key.clone()))
                .collect();
            let cur = keys.iter().position(|c| *c == s.forward_pad);
            step_option(cur, keys.len(), delta, wrap).map(|i| s.forward_pad = keys[i].clone())
        }
        RowId::PadType => step_str(&PAD_TYPES, &mut s.gamepad, delta, wrap),
        RowId::Stats => {
            let cur = StatsVerbosity::ALL
                .iter()
                .position(|v| *v == s.stats_verbosity());
            step_option(cur, StatsVerbosity::ALL.len(), delta, wrap)
                .map(|i| s.set_stats_verbosity(StatsVerbosity::ALL[i]))
        }
    }
    .is_some()
}

/// The shared stepping rule: clamp when adjusting, wrap when cycling; an unknown
/// current value snaps to the first option on any step.
fn step_option(current: Option<usize>, len: usize, delta: i32, wrap: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let Some(cur) = current else { return Some(0) };
    let target = cur as i32 + delta;
    if wrap {
        Some(target.rem_euclid(len as i32) as usize)
    } else if target < 0 || target >= len as i32 {
        None
    } else {
        Some(target as usize)
    }
}

fn step_str(options: &[(&str, &str)], value: &mut String, delta: i32, wrap: bool) -> Option<()> {
    let cur = options.iter().position(|(v, _)| v == value);
    step_option(cur, options.len(), delta, wrap).map(|i| *value = options[i].0.to_string())
}

fn toggle(value: &mut bool, delta: i32, wrap: bool) -> Option<()> {
    let target = if wrap { !*value } else { delta > 0 };
    if *value == target {
        None
    } else {
        *value = target;
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_client_core::trust::Settings;

    fn ctx_parts() -> (Settings, Vec<pf_client_core::gamepad::PadInfo>) {
        (Settings::default(), Vec::new())
    }

    #[test]
    fn adjust_clamps_and_activate_wraps() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            pads: &pads,
            deck: false,
            device_name: "t",
            t: 0.0,
        };
        // Resolution starts at Native (index 0): left refuses, right steps — first onto
        // Match window (the D1 tri-state's middle option), then the explicit sizes.
        assert!(!adjust(RowId::Resolution, -1, false, &mut ctx));
        assert!(adjust(RowId::Resolution, 1, false, &mut ctx));
        assert!(ctx.settings.match_window, "Native → Match window");
        assert_eq!((ctx.settings.width, ctx.settings.height), (0, 0));
        assert!(adjust(RowId::Resolution, 1, false, &mut ctx));
        assert!(!ctx.settings.match_window, "explicit size clears the policy");
        assert_eq!((ctx.settings.width, ctx.settings.height), (1280, 720));
        // Stepping back from an explicit size returns to Match window, then Native.
        assert!(adjust(RowId::Resolution, -1, false, &mut ctx));
        assert!(ctx.settings.match_window);
        assert!(adjust(RowId::Resolution, -1, false, &mut ctx));
        assert!(!ctx.settings.match_window);
        assert_eq!(ctx.settings.width, 0, "back to Native");
        // Cycle from the last option wraps to the first.
        (ctx.settings.width, ctx.settings.height) = (3840, 2160);
        assert!(adjust(RowId::Resolution, 1, true, &mut ctx));
        assert_eq!(ctx.settings.width, 0, "wrapped to Native");
        assert!(!ctx.settings.match_window);
    }

    #[test]
    fn toggles_read_left_off_right_on() {
        let (mut settings, pads) = ctx_parts();
        settings.mic_enabled = false;
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            pads: &pads,
            deck: false,
            device_name: "t",
            t: 0.0,
        };
        assert!(
            !adjust(RowId::Mic, -1, false, &mut ctx),
            "already off = thud"
        );
        assert!(adjust(RowId::Mic, 1, false, &mut ctx));
        assert!(ctx.settings.mic_enabled);
        assert!(adjust(RowId::Mic, 1, true, &mut ctx), "A always flips");
        assert!(!ctx.settings.mic_enabled);
    }

    #[test]
    fn unknown_value_snaps_to_first() {
        let (mut settings, pads) = ctx_parts();
        settings.bitrate_kbps = 12_345; // set via a desktop shell's free-form field
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            pads: &pads,
            deck: false,
            device_name: "t",
            t: 0.0,
        };
        assert!(adjust(RowId::Bitrate, 1, false, &mut ctx));
        assert_eq!(ctx.settings.bitrate_kbps, 0, "snapped to Automatic");
    }
}
