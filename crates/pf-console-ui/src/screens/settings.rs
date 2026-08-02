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
use pf_client_core::trust::{MouseMode, StatsVerbosity, TouchMode};
use skia_safe::{Canvas, Rect};

/// Stable row identity — adjust/activate dispatch by id so nothing acts on a stale
/// index when the pad list under the "Use controller" row churns.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowId {
    Resolution,
    Refresh,
    RenderScale,
    Bitrate,
    Compositor,
    Codec,
    Decoder,
    Hdr,
    Chroma444,
    PresentPriority,
    SmoothBuffer,
    Audio,
    Mic,
    EchoCancel,
    PadForward,
    Pad,
    PadType,
    Touch,
    Mouse,
    InvertScroll,
    Shortcuts,
    Stats,
    Fullscreen,
    AutoWake,
    Library,
}

// The couch-relevant subset grew 2026-07-31: this screen is the ONLY settings editor in
// Gaming Mode, so a field it omits is simply unreachable there (render scale, 4:4:4,
// scroll/shortcut behavior, fullscreen-on-stream, auto-wake, the library toggle and echo
// cancellation all were). Still deliberately smaller than the desktop dialogs — device
// pickers (GPU/speaker/mic) and the profile catalog stay desktop-only.
const ROWS: [RowId; 25] = [
    RowId::Resolution,
    RowId::Refresh,
    RowId::RenderScale,
    RowId::Bitrate,
    RowId::Compositor,
    RowId::Codec,
    RowId::Decoder,
    RowId::Hdr,
    RowId::Chroma444,
    RowId::PresentPriority,
    RowId::SmoothBuffer,
    RowId::Audio,
    RowId::Mic,
    RowId::EchoCancel,
    RowId::PadForward,
    RowId::Pad,
    RowId::PadType,
    RowId::Touch,
    RowId::Mouse,
    RowId::InvertScroll,
    RowId::Shortcuts,
    RowId::Stats,
    RowId::Fullscreen,
    RowId::AutoWake,
    RowId::Library,
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
/// Mirrors [`punktfunk_core::render_scale::PRESETS`] (and the desktop pickers).
const RENDER_SCALES: [f64; 9] = [0.5, 0.67, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0];
const BITRATES: [u32; 7] = [0, 5_000, 10_000, 20_000, 30_000, 50_000, 80_000];
const COMPOSITORS: [(&str, &str); 5] = [
    ("auto", "Automatic"),
    ("kwin", "KWin"),
    ("wlroots", "wlroots"),
    ("mutter", "Mutter"),
    ("gamescope", "gamescope"),
];
const CODECS: [(&str, &str); 5] = [
    ("auto", "Automatic"),
    ("hevc", "HEVC"),
    ("h264", "H.264"),
    ("av1", "AV1"),
    // Opt-in wired-LAN low-latency codec (100-400 Mbps class, 8-bit SDR). Only ever
    // selected when the host supports it too; anything else falls back to HEVC.
    ("pyrowave", "PyroWave (wired LAN)"),
];
// Per-OS hardware rungs, like the shells' pickers: the console ships on Windows too
// (`punktfunk-session --browse`), where "vaapi" is a dead option that ALSO hid the real
// hardware path (d3d11va) — `Decoder::new` has no VAAPI branch there.
#[cfg(not(windows))]
const DECODERS: [(&str, &str); 4] = [
    ("auto", "Automatic"),
    ("vulkan", "Vulkan Video"),
    ("vaapi", "VAAPI"),
    ("software", "Software"),
];
#[cfg(windows)]
const DECODERS: [(&str, &str); 4] = [
    ("auto", "Automatic"),
    ("vulkan", "Vulkan Video"),
    ("d3d11va", "Direct3D 11"),
    ("software", "Software"),
];
const AUDIO: [(u8, &str); 3] = [(2, "Stereo"), (6, "5.1"), (8, "7.1")];
/// Presentation intent — the `present_priority` key shared with the Apple and Android
/// clients, so one profile reads the same on every device.
const PRESENT_PRIORITIES: [(&str, &str); 2] =
    [("latency", "Lowest latency"), ("smooth", "Smoothness")];
/// Smoothness buffer depth in frames; `0` = Automatic (resolves to 2).
const SMOOTH_BUFFERS: [(u8, &str); 4] = [
    (0, "Automatic"),
    (1, "1 frame"),
    (2, "2 frames"),
    (3, "3 frames"),
];
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
        // Rebase the shell-lifetime snapshot on the file before an adjust-then-save: this
        // screen is one of the settings file's several whole-file writers (profiles.rs
        // documents the no-merge debt), and adjusting a stale snapshot would silently
        // revert what another writer (a session's match-window persist, a desktop shell)
        // stored while the console was open. Only on the mutating events — a cursor move
        // shouldn't touch the disk.
        if matches!(msg, ListMsg::Adjust(_) | ListMsg::Activate) {
            *ctx.settings = pf_client_core::trust::Settings::load();
        }
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
    // Several rows follow another: echo cancellation only means anything while the mic
    // streams, the pad rows only while any controller is forwarded at all, and the
    // smoothness buffer only while that intent is chosen. All go dim and inert otherwise
    // — the same relationship the desktop shells draw by greying a row out (they hide the
    // buffer row entirely; a fixed row list can't, and a row that vanished mid-list would
    // move everything under the cursor).
    let enabled = match id {
        RowId::EchoCancel => s.mic_enabled,
        RowId::Pad | RowId::PadType => s.gamepad_forwarding,
        RowId::SmoothBuffer => s.present_priority == "smooth",
        _ => true,
    };
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
        RowId::RenderScale => (
            None,
            "Render scale",
            if s.render_scale == 1.0 {
                "Native".into()
            } else if s.render_scale > 1.0 {
                format!("{}× (supersample)", s.render_scale)
            } else {
                format!("{}×", s.render_scale)
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
        RowId::Chroma444 => (None, "Full chroma (4:4:4)", on_off(s.enable_444).into()),
        RowId::PresentPriority => (
            Some("Presentation"),
            "Prioritize",
            label_for(&PRESENT_PRIORITIES, &s.present_priority).into(),
        ),
        RowId::SmoothBuffer => (
            None,
            "Smoothness buffer",
            SMOOTH_BUFFERS
                .iter()
                .find(|(v, _)| *v == s.smooth_buffer)
                .map_or("Automatic", |(_, l)| l)
                .into(),
        ),
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
        RowId::EchoCancel => (None, "Echo cancellation", on_off(s.echo_cancel).into()),
        RowId::PadForward => (
            Some("Controller"),
            "Forward controllers",
            on_off(s.gamepad_forwarding).into(),
        ),
        RowId::Pad => (
            None,
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
        RowId::Touch => (
            Some("Touchscreen"),
            "Touch mode",
            s.touch_mode().label().into(),
        ),
        RowId::Mouse => (None, "Mouse mode", s.mouse_mode().label().into()),
        RowId::InvertScroll => (None, "Invert scroll", on_off(s.invert_scroll).into()),
        RowId::Shortcuts => (
            None,
            "Capture system shortcuts",
            on_off(s.inhibit_shortcuts).into(),
        ),
        RowId::Stats => (
            Some("Interface"),
            "Statistics overlay",
            s.stats_verbosity().label().into(),
        ),
        RowId::Fullscreen => (
            None,
            "Start streams fullscreen",
            on_off(s.fullscreen_on_stream).into(),
        ),
        RowId::AutoWake => (None, "Wake hosts automatically", on_off(s.auto_wake).into()),
        RowId::Library => (None, "Game library", on_off(s.library_enabled).into()),
    };
    RowSpec {
        header,
        label: label.into(),
        value: Some(value),
        value_dim: !enabled,
        caret: false,
        adjustable: enabled,
        enabled,
    }
}

fn detail(id: RowId) -> &'static str {
    match id {
        RowId::Resolution => {
            "The host creates a virtual display at exactly this size — no scaling. \
             Match window follows this window, including mid-stream resizes."
        }
        RowId::Refresh => "Native follows the display this window is on.",
        RowId::RenderScale => {
            "The host renders larger or smaller than the stream mode and this window \
             resamples — above 1× supersamples, below saves bandwidth."
        }
        RowId::Bitrate => "Automatic uses the host's default (20 Mbps).",
        RowId::Compositor => {
            "Which compositor drives the virtual output — honored only if available on the host."
        }
        RowId::Codec => "A preference — the host falls back if it can't encode this one.",
        RowId::Decoder => "Automatic prefers Vulkan Video, then VAAPI, then software.",
        RowId::Hdr => {
            "HDR10 — engages when the host sends HDR content and this display supports it."
        }
        RowId::Chroma444 => {
            "Full-colour video: crisp small text and thin lines, at more bandwidth. \
             Needs an NVIDIA host (NVENC) or the PyroWave codec — other encoders \
             stream 4:2:0 and the session falls back silently."
        }
        RowId::PresentPriority => {
            "Lowest latency shows each frame the moment the display can take it — a \
             network hiccup becomes an occasional repeated or skipped frame. Smoothness \
             buffers a little to even those out."
        }
        RowId::SmoothBuffer => {
            "Frames held back before showing. Each one absorbs about a refresh of network \
             hiccup and adds a refresh of delay. Automatic holds two."
        }
        RowId::Audio => "The speaker layout requested from the host.",
        RowId::Mic => {
            "Send this device's microphone to the host's virtual mic. \
             Ctrl+Alt+Shift+V mutes and unmutes it while streaming."
        }
        RowId::EchoCancel => {
            "Stops the host's audio, playing from this device's speakers, being picked up \
             and sent back. Turn it off if your microphone already runs its own processing."
        }
        RowId::PadForward => {
            "Send controllers connected to this device to the host. Turn it off when your \
             controller already reaches the host another way — USB passthrough such as \
             VirtualHere, or a pad plugged into the host — so games don't see two of them."
        }
        RowId::Pad => "Which pad is forwarded to the host, as player 1.",
        RowId::PadType => "The virtual pad the host creates — Automatic matches this controller.",
        RowId::Touch => {
            "How the touchscreen drives the host: Trackpad (relative cursor), \
             Direct pointer (cursor jumps to your finger), or Touch passthrough (raw contacts)."
        }
        RowId::Mouse => {
            "How a physical mouse drives the host: Capture locks the pointer (relative, \
             for games), Desktop leaves it free and sends absolute positions. \
             Ctrl+Alt+Shift+M switches live while streaming."
        }
        RowId::InvertScroll => "Reverses the wheel and trackpad scroll direction sent to the host.",
        RowId::Shortcuts => {
            "Alt+Tab, Super and friends reach the host while input is captured. \
             Off, they act on this device instead."
        }
        RowId::Stats => {
            "How much the overlay shows: Compact (one line) → Normal → Detailed. \
             Ctrl+Alt+Shift+S cycles it live while streaming."
        }
        RowId::Fullscreen => "Streams open fullscreen instead of windowed.",
        RowId::AutoWake => {
            "Send Wake-on-LAN to a sleeping host before connecting. Turn off for hosts \
             reached over a VPN, where the wake wait only adds delay."
        }
        RowId::Library => "Show paired hosts' game libraries (tap a title to stream it).",
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
        RowId::RenderScale => {
            // Exact float compare is fine: every writer (here and the desktop pickers)
            // stores one of these literals; a hand-edited oddball snaps to the first step.
            let cur = RENDER_SCALES.iter().position(|v| *v == s.render_scale);
            step_option(cur, RENDER_SCALES.len(), delta, wrap)
                .map(|i| s.render_scale = RENDER_SCALES[i])
        }
        RowId::Bitrate => {
            let cur = BITRATES.iter().position(|b| *b == s.bitrate_kbps);
            step_option(cur, BITRATES.len(), delta, wrap).map(|i| s.bitrate_kbps = BITRATES[i])
        }
        RowId::Compositor => step_str(&COMPOSITORS, &mut s.compositor, delta, wrap),
        RowId::Codec => step_str(&CODECS, &mut s.codec, delta, wrap),
        RowId::Decoder => step_str(&DECODERS, &mut s.decoder, delta, wrap),
        RowId::Hdr => toggle(&mut s.hdr_enabled, delta, wrap),
        RowId::Chroma444 => toggle(&mut s.enable_444, delta, wrap),
        RowId::PresentPriority => {
            let cur = PRESENT_PRIORITIES
                .iter()
                .position(|(v, _)| *v == s.present_priority);
            step_option(cur, PRESENT_PRIORITIES.len(), delta, wrap)
                .map(|i| s.present_priority = PRESENT_PRIORITIES[i].0.to_string())
        }
        // Inert unless smoothness is chosen — a boundary thud, matching the dimmed row.
        RowId::SmoothBuffer => {
            if s.present_priority == "smooth" {
                let cur = SMOOTH_BUFFERS
                    .iter()
                    .position(|(v, _)| *v == s.smooth_buffer);
                step_option(cur, SMOOTH_BUFFERS.len(), delta, wrap)
                    .map(|i| s.smooth_buffer = SMOOTH_BUFFERS[i].0)
            } else {
                None
            }
        }
        RowId::Audio => {
            let cur = AUDIO.iter().position(|(v, _)| *v == s.audio_channels);
            step_option(cur, AUDIO.len(), delta, wrap).map(|i| s.audio_channels = AUDIO[i].0)
        }
        RowId::Mic => toggle(&mut s.mic_enabled, delta, wrap),
        // Inert while the mic is off — a boundary thud, matching what the dimmed row shows.
        RowId::EchoCancel => {
            if s.mic_enabled {
                toggle(&mut s.echo_cancel, delta, wrap)
            } else {
                None
            }
        }
        RowId::PadForward => toggle(&mut s.gamepad_forwarding, delta, wrap),
        RowId::Pad => {
            if !s.gamepad_forwarding {
                return false;
            }
            // Automatic first, then every connected pad by stable key.
            let keys: Vec<String> = std::iter::once(String::new())
                .chain(ctx.pads.iter().map(|p| p.key.clone()))
                .collect();
            let cur = keys.iter().position(|c| *c == s.forward_pad);
            step_option(cur, keys.len(), delta, wrap).map(|i| s.forward_pad = keys[i].clone())
        }
        RowId::PadType => {
            if !s.gamepad_forwarding {
                return false;
            }
            step_str(&PAD_TYPES, &mut s.gamepad, delta, wrap)
        }
        RowId::Touch => {
            let cur = TouchMode::ALL.iter().position(|m| *m == s.touch_mode());
            step_option(cur, TouchMode::ALL.len(), delta, wrap)
                .map(|i| s.touch_mode = TouchMode::ALL[i].as_name().to_string())
        }
        RowId::Mouse => {
            let cur = MouseMode::ALL.iter().position(|m| *m == s.mouse_mode());
            step_option(cur, MouseMode::ALL.len(), delta, wrap)
                .map(|i| s.mouse_mode = MouseMode::ALL[i].as_name().to_string())
        }
        RowId::InvertScroll => toggle(&mut s.invert_scroll, delta, wrap),
        RowId::Shortcuts => toggle(&mut s.inhibit_shortcuts, delta, wrap),
        RowId::Stats => {
            let cur = StatsVerbosity::ALL
                .iter()
                .position(|v| *v == s.stats_verbosity());
            step_option(cur, StatsVerbosity::ALL.len(), delta, wrap)
                .map(|i| s.set_stats_verbosity(StatsVerbosity::ALL[i]))
        }
        RowId::Fullscreen => toggle(&mut s.fullscreen_on_stream, delta, wrap),
        RowId::AutoWake => toggle(&mut s.auto_wake, delta, wrap),
        RowId::Library => toggle(&mut s.library_enabled, delta, wrap),
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
        assert!(
            !ctx.settings.match_window,
            "explicit size clears the policy"
        );
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

    /// Echo cancellation follows the microphone: inert and dimmed while the mic is off, live
    /// the moment it goes on. A row that silently accepted a change nobody could act on would
    /// be the same lie as an enabled-looking control.
    #[test]
    fn echo_cancellation_follows_the_microphone() {
        let (mut settings, pads) = ctx_parts();
        settings.mic_enabled = false;
        assert!(settings.echo_cancel, "it ships on");
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
        assert!(!row_spec(RowId::EchoCancel, &ctx).enabled);
        assert!(
            !adjust(RowId::EchoCancel, -1, false, &mut ctx),
            "mic off = thud"
        );
        assert!(!adjust(RowId::EchoCancel, 1, true, &mut ctx), "A too");
        assert!(ctx.settings.echo_cancel, "and nothing was written");

        ctx.settings.mic_enabled = true;
        assert!(row_spec(RowId::EchoCancel, &ctx).enabled);
        assert!(adjust(RowId::EchoCancel, -1, false, &mut ctx));
        assert!(!ctx.settings.echo_cancel);
        assert!(adjust(RowId::EchoCancel, 1, true, &mut ctx));
        assert!(ctx.settings.echo_cancel);
    }

    /// The smoothness buffer follows the presentation intent, exactly as echo cancellation
    /// follows the mic: dimmed and inert under Lowest latency (where holding frames means
    /// nothing), live under Smoothness. The desktop shells hide the row instead; a fixed
    /// row list dims it, because a row vanishing mid-list would shift everything under the
    /// cursor.
    #[test]
    fn smoothness_buffer_follows_the_intent() {
        let (mut settings, pads) = ctx_parts();
        assert_eq!(settings.present_priority, "latency", "the shipped default");
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
        assert!(!row_spec(RowId::SmoothBuffer, &ctx).enabled);
        assert!(
            !adjust(RowId::SmoothBuffer, 1, false, &mut ctx),
            "latency intent = thud"
        );
        assert_eq!(ctx.settings.smooth_buffer, 0, "and nothing was written");

        // Stepping the intent to Smoothness brings the buffer row to life.
        assert!(adjust(RowId::PresentPriority, 1, false, &mut ctx));
        assert_eq!(ctx.settings.present_priority, "smooth");
        assert!(row_spec(RowId::SmoothBuffer, &ctx).enabled);
        assert!(adjust(RowId::SmoothBuffer, 1, false, &mut ctx));
        assert_eq!(ctx.settings.smooth_buffer, 1);

        // The intent wraps back and the row goes inert again.
        assert!(adjust(RowId::PresentPriority, -1, false, &mut ctx));
        assert_eq!(ctx.settings.present_priority, "latency");
        assert!(!row_spec(RowId::SmoothBuffer, &ctx).enabled);
    }

    #[test]
    fn touch_mode_steps_and_wraps() {
        let (mut settings, pads) = ctx_parts();
        assert_eq!(settings.touch_mode, "trackpad");
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
        // Trackpad → Pointer → Touch, then a step past the end is a boundary.
        assert!(
            !adjust(RowId::Touch, -1, false, &mut ctx),
            "already first = thud"
        );
        assert!(adjust(RowId::Touch, 1, false, &mut ctx));
        assert_eq!(ctx.settings.touch_mode, "pointer");
        assert!(adjust(RowId::Touch, 1, false, &mut ctx));
        assert_eq!(ctx.settings.touch_mode, "touch");
        assert!(!adjust(RowId::Touch, 1, false, &mut ctx), "last = thud");
        // A wraps back to the first.
        assert!(adjust(RowId::Touch, 1, true, &mut ctx));
        assert_eq!(ctx.settings.touch_mode, "trackpad");
    }

    #[test]
    fn mouse_mode_steps_and_wraps() {
        let (mut settings, pads) = ctx_parts();
        assert_eq!(settings.mouse_mode, "capture");
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
        // Capture → Desktop, then a step past the end is a boundary.
        assert!(
            !adjust(RowId::Mouse, -1, false, &mut ctx),
            "already first = thud"
        );
        assert!(adjust(RowId::Mouse, 1, false, &mut ctx));
        assert_eq!(ctx.settings.mouse_mode, "desktop");
        assert!(!adjust(RowId::Mouse, 1, false, &mut ctx), "last = thud");
        // A wraps back to the first.
        assert!(adjust(RowId::Mouse, 1, true, &mut ctx));
        assert_eq!(ctx.settings.mouse_mode, "capture");
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
