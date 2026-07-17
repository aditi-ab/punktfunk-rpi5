//! The settings screen. Every control writes straight back to the persisted [`Settings`]
//! (there is no Apply step), via the small [`setting_combo`]/[`setting_toggle`] builders.

use super::style::*;
use super::{AppCtx, Screen};
use crate::trust::Settings;
use pf_client_core::trust::StatsVerbosity;
use punktfunk_core::config::GamepadPref;
use std::sync::Arc;
use windows_reactor::*;

/// `(0, 0)` = the native size of the display the window is on, resolved at connect.
const RESOLUTIONS: &[(u32, u32)] = &[
    (0, 0),
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];
/// `0` = the display's native refresh, resolved at connect.
const REFRESH: &[u32] = &[0, 30, 60, 90, 120, 144, 165, 240];
/// Render-scale multipliers (persisted as f64; mirrors [`punktfunk_core::render_scale::PRESETS`]).
/// `1.0` = Native. Applied at connect and each match-window resize.
const RENDER_SCALES: &[f64] = &[0.5, 0.67, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0];

/// A compact label for a render-scale multiplier: "Native" / "1.5×" / "2× (supersample)".
fn render_scale_label(scale: f64) -> String {
    if scale == 1.0 {
        "Native".to_string()
    } else if scale > 1.0 {
        format!("{scale}\u{00D7} (supersample)")
    } else {
        format!("{scale}\u{00D7}")
    }
}
/// Decode backend presets: `(stored value, display label)`.
// A stored legacy "hardware" (the D3D11VA era) matches no preset, so the combo shows
// Automatic — which is exactly how the session's decoder chain reads that value.
const DECODERS: &[(&str, &str)] = &[
    ("auto", "Automatic (GPU, fall back to CPU)"),
    ("vulkan", "Hardware (GPU / Vulkan Video)"),
    ("software", "Software (CPU)"),
];
/// Audio channel presets: `(channel count, display label)`. The host clamps to what it can
/// capture; the resolved count drives the decoder + WASAPI render layout.
const AUDIO_CHANNELS: &[(u8, &str)] = &[(2, "Stereo"), (6, "5.1 Surround"), (8, "7.1 Surround")];
/// Preferred-codec presets: `(stored value, display label)`. Soft — the host falls back if it
/// can't encode the chosen codec.
const CODECS: &[(&str, &str)] = &[
    ("auto", "Automatic"),
    ("hevc", "HEVC (H.265)"),
    ("h264", "H.264 (AVC)"),
    ("av1", "AV1"),
];
/// Virtual-pad presets: `(stored value, display label)` — the pad the HOST creates. Same set the
/// GTK client offers; "Automatic" resolves from the physical controller at connect.
const GAMEPADS: &[(&str, &str)] = &[
    ("auto", "Automatic (match the controller)"),
    ("xbox360", "Xbox 360"),
    ("dualsense", "DualSense"),
    ("xboxone", "Xbox One"),
    ("dualshock4", "DualShock 4"),
];
/// Stats-overlay tiers: `(stored value, display label)` — the cross-client verbosity ladder
/// (Compact ⊂ Normal ⊂ Detailed); Ctrl+Alt+Shift+S cycles it live in the session window.
const STATS_TIERS: &[(StatsVerbosity, &str)] = &[
    (StatsVerbosity::Off, "Off"),
    (StatsVerbosity::Compact, "Compact"),
    (StatsVerbosity::Normal, "Normal"),
    (StatsVerbosity::Detailed, "Detailed"),
];
/// Touch-input presets: `(stored value, display label)` — how a touchscreen's fingers drive
/// the host. The cross-client set (Android/Apple); only meaningful on a touchscreen device.
const TOUCH_MODES: &[(&str, &str)] = &[
    ("trackpad", "Trackpad"),
    ("pointer", "Direct pointer"),
    ("touch", "Touch passthrough"),
];
/// Host compositor presets: `(stored value, display label)`. Advisory — the host falls back to
/// auto-detect when the choice is unavailable. Only meaningful against a Linux host.
const COMPOSITORS: &[(&str, &str)] = &[
    ("auto", "Automatic"),
    ("kwin", "KWin"),
    ("wlroots", "wlroots (Sway/Hyprland)"),
    ("mutter", "Mutter (GNOME)"),
    ("gamescope", "gamescope"),
];

/// A `ComboBox` bound to one settings field: shows `names`, starts at `current`, and runs
/// `apply(settings, picked_index)` under the settings lock, then saves. The index handed to
/// `apply` is already clamped to `names`.
fn setting_combo(
    ctx: &Arc<AppCtx>,
    header: &str,
    names: Vec<String>,
    current: usize,
    apply: impl Fn(&mut Settings, usize) + 'static,
) -> ComboBox {
    let ctx = ctx.clone();
    let max = names.len().saturating_sub(1);
    ComboBox::new(names)
        .header(header)
        .selected_index(current as i32)
        .on_selection_changed(move |i: i32| {
            let mut s = ctx.settings.lock().unwrap();
            apply(&mut s, (i.max(0) as usize).min(max));
            s.save();
        })
}

/// The labels of a `(value, label)` preset table, plus the index of `is_current`'s match.
fn presets<V>(table: &[(V, &str)], is_current: impl Fn(&V) -> bool) -> (Vec<String>, usize) {
    let names = table.iter().map(|(_, l)| l.to_string()).collect();
    let current = table.iter().position(|(v, _)| is_current(v)).unwrap_or(0);
    (names, current)
}

/// A `ToggleSwitch` bound to one boolean settings field.
fn setting_toggle(
    ctx: &Arc<AppCtx>,
    header: &str,
    on: bool,
    apply: impl Fn(&mut Settings, bool) + 'static,
) -> ToggleSwitch {
    let ctx = ctx.clone();
    ToggleSwitch::new(on)
        .header(header)
        .on_content("On")
        .off_content("Off")
        .on_toggled(move |v: bool| {
            let mut s = ctx.settings.lock().unwrap();
            apply(&mut s, v);
            s.save();
        })
}

/// A settings card: just the controls. No heading (the section title is the NavigationView
/// header) and no description paragraph — per-control guidance is a `.tooltip(...)` on the
/// control itself (a paragraph in the card reads as the first control's label).
fn settings_card(controls: Vec<Element>) -> Element {
    card(vstack(controls).spacing(10.0)).into()
}

/// The settings screen: a stock WinUI `NavigationView` (the Windows-Settings sidebar pattern) —
/// one pane item per section, the section's card as the content, the built-in back arrow
/// returning to the host list. `section`/`set_section` are the selected pane tag, held in ROOT
/// state (this page stays hook-free): `on_selection_changed` is wired in the reactor backend, so
/// only a root `AsyncSetState` reliably re-renders the new section in. `progress` is the
/// section-switch entrance tween (0 → 1), mapped onto the content column's opacity + offset.
pub(crate) fn settings_page(
    ctx: &Arc<AppCtx>,
    set_screen: &AsyncSetState<Screen>,
    section: &str,
    set_section: &AsyncSetState<String>,
    progress: f64,
) -> Element {
    let s = ctx.settings.lock().unwrap().clone();

    // --- Display ---------------------------------------------------------------------------
    // The D1 tri-state: Native, Match window (a virtual index 1, stored as the
    // `match_window` flag), then the explicit sizes.
    let (res_names, res_i) = {
        let names: Vec<String> = std::iter::once("Native display".to_string())
            .chain(std::iter::once("Match window".to_string()))
            .chain(
                RESOLUTIONS
                    .iter()
                    .skip(1)
                    .map(|&(w, h)| format!("{w} \u{00D7} {h}")),
            )
            .collect();
        let i = if s.match_window {
            1
        } else {
            RESOLUTIONS
                .iter()
                .position(|&(w, h)| w == s.width && h == s.height)
                .map(|i| if i == 0 { 0 } else { i + 1 })
                .unwrap_or(0)
        };
        (names, i)
    };
    let res_combo = setting_combo(ctx, "Resolution", res_names, res_i, |s, i| {
        s.match_window = i == 1;
        (s.width, s.height) = if i <= 1 { (0, 0) } else { RESOLUTIONS[i - 1] };
    })
    .tooltip(
        "The host creates a virtual display at exactly this size. \u{201C}Native display\u{201D} \
         resolves to the monitor this window is on at connect; \u{201C}Match window\u{201D} \
         follows the stream window, including mid-stream resizes.",
    );
    let (hz_names, hz_i) = {
        let names: Vec<String> = REFRESH
            .iter()
            .map(|&r| {
                if r == 0 {
                    "Native".into()
                } else {
                    format!("{r} Hz")
                }
            })
            .collect();
        let i = REFRESH.iter().position(|&r| r == s.refresh_hz).unwrap_or(0);
        (names, i)
    };
    let hz_combo = setting_combo(ctx, "Refresh rate", hz_names, hz_i, |s, i| {
        s.refresh_hz = REFRESH[i];
    })
    .tooltip("\u{201C}Native\u{201D} resolves to this display's refresh rate at connect.");
    let (scale_names, scale_i) = {
        let names: Vec<String> = RENDER_SCALES
            .iter()
            .map(|&x| render_scale_label(x))
            .collect();
        let i = RENDER_SCALES
            .iter()
            .position(|&x| (x - s.render_scale).abs() < 1e-6)
            .unwrap_or_else(|| RENDER_SCALES.iter().position(|&x| x == 1.0).unwrap());
        (names, i)
    };
    let scale_combo = setting_combo(ctx, "Render scale", scale_names, scale_i, |s, i| {
        s.render_scale = RENDER_SCALES[i];
    })
    .tooltip(
        "Supersample for sharpness (above 1\u{00D7}, more bandwidth and decode) or render below \
         native (below 1\u{00D7}) for a lighter host \u{2014} this device resamples to the window.",
    );
    let (comp_names, comp_i) = presets(COMPOSITORS, |v| *v == s.compositor);
    let comp_combo = setting_combo(ctx, "Host compositor", comp_names, comp_i, |s, i| {
        s.compositor = COMPOSITORS[i].0.to_string();
    })
    .tooltip(
        "Linux hosts only, and advisory \u{2014} the host falls back to auto-detect when the \
         choice is unavailable.",
    );
    let fullscreen_toggle = setting_toggle(
        ctx,
        "Start streams fullscreen",
        s.fullscreen_on_stream,
        |s, on| s.fullscreen_on_stream = on,
    )
    .tooltip("The stream window opens fullscreen; F11 or Alt+Enter switches back live.");

    // --- Video -----------------------------------------------------------------------------
    let (dec_names, dec_i) = presets(DECODERS, |v| *v == s.decoder);
    let decoder_combo = setting_combo(ctx, "Video decoder", dec_names, dec_i, |s, i| {
        s.decoder = DECODERS[i].0.to_string();
    })
    .tooltip(
        "Hardware decode (Vulkan Video) is far lighter than software \u{2014} keep it on \
         Automatic unless debugging.",
    );
    // GPU picker, only on a multi-GPU box (hybrid laptop, eGPU): which adapter decodes + presents.
    // Stored as the adapter description; empty = automatic (the window's monitor's adapter).
    let gpus = crate::gpu::adapter_names();
    let gpu_combo = (gpus.len() > 1).then(|| {
        let mut names = vec!["Automatic (the display's GPU)".to_string()];
        names.extend(gpus.iter().cloned());
        let current = gpus
            .iter()
            .position(|n| *n == s.adapter)
            .map_or(0, |i| i + 1);
        let gpus = gpus.clone();
        setting_combo(ctx, "GPU", names, current, move |s, i| {
            s.adapter = if i == 0 {
                String::new()
            } else {
                gpus[i - 1].clone()
            };
        })
        .tooltip(
            "Which adapter decodes and presents the stream. Applies to the next stream; \
             Automatic uses the GPU driving this window's display.",
        )
    });
    let (codec_names, codec_i) = presets(CODECS, |v| *v == s.codec);
    let codec_combo = setting_combo(ctx, "Video codec", codec_names, codec_i, |s, i| {
        s.codec = CODECS[i].0.to_string();
    })
    .tooltip(
        "A soft preference \u{2014} the host falls back to the best codec both sides support.",
    );
    // Free-form Mb/s (0 = host default) instead of presets, so a speed-test recommendation
    // round-trips exactly.
    let bitrate_box = {
        let ctx = ctx.clone();
        NumberBox::new(f64::from(s.bitrate_kbps) / 1000.0)
            .header("Bitrate (Mb/s, 0 = automatic)")
            .range(0.0, 3000.0)
            .on_value_changed(move |v: f64| {
                let mut s = ctx.settings.lock().unwrap();
                s.bitrate_kbps = (v.clamp(0.0, 3000.0) * 1000.0) as u32;
                s.save();
            })
            .tooltip(
                "0 lets the host decide. Run a per-host speed test from the host list for a \
                 recommendation.",
            )
    };
    let hdr_toggle = setting_toggle(ctx, "HDR (10-bit, BT.2020 PQ)", s.hdr_enabled, |s, on| {
        s.hdr_enabled = on
    })
    .tooltip(
        "Advertise 10-bit HDR10 so the host upgrades HDR content. Needs a display in HDR mode; \
         SDR content is unaffected.",
    );

    // --- Input -----------------------------------------------------------------------------
    // Controller forwarding: Automatic forwards EVERY real controller, each as its own pad;
    // pinning one restricts the session to that single controller (single-player). Persisted
    // by stable key (`Settings::forward_pad`, GTK parity) so the pin survives restarts AND
    // reaches the spawned session binary, whose service applies the same key.
    let pads = ctx.gamepad.pads();
    let (fwd_names, fwd_i) = {
        let mut names = vec!["Automatic (all controllers)".to_string()];
        names.extend(pads.iter().map(|p| {
            let kind = p.kind_label();
            if kind.is_empty() {
                p.name.clone()
            } else {
                format!("{} \u{00B7} {kind}", p.name)
            }
        }));
        let i = (!s.forward_pad.is_empty())
            .then(|| pads.iter().position(|p| p.key == s.forward_pad))
            .flatten()
            .map_or(0, |i| i + 1);
        (names, i)
    };
    let forward_combo = {
        let svc = ctx.gamepad.clone();
        let ctx2 = ctx.clone();
        let keys: Vec<String> = pads.iter().map(|p| p.key.clone()).collect();
        ComboBox::new(fwd_names)
            .header("Forwarded controller")
            .selected_index(fwd_i as i32)
            .on_selection_changed(move |i: i32| {
                let sel = i.max(0) as usize;
                let key = if sel == 0 {
                    None
                } else {
                    keys.get(sel - 1).cloned()
                };
                // Apply live to the gamepad service and persist — the spawned session
                // reads `forward_pad` at connect.
                svc.set_pinned(key.clone());
                let mut s = ctx2.settings.lock().unwrap();
                s.forward_pad = key.unwrap_or_default();
                s.save();
            })
            .tooltip(
                "Every connected controller is forwarded, each as its own player. Pick one \
                 to force single-player \u{2014} only it reaches the host.",
            )
    };
    let (pad_names, pad_i) = presets(GAMEPADS, |v| {
        GamepadPref::from_name(v) == GamepadPref::from_name(&s.gamepad)
    });
    let pad_combo = setting_combo(ctx, "Gamepad type", pad_names, pad_i, |s, i| {
        s.gamepad = GAMEPADS[i].0.to_string();
    })
    .tooltip(
        "The virtual pad the host creates. \u{201C}Automatic\u{201D} matches your physical \
         controller.",
    );
    let (touch_names, touch_i) = presets(TOUCH_MODES, |v| *v == s.touch_mode);
    let touch_combo = setting_combo(ctx, "Touch input", touch_names, touch_i, |s, i| {
        s.touch_mode = TOUCH_MODES[i].0.to_string();
    })
    .tooltip(
        "How a touchscreen drives the host: Trackpad nudges a cursor (tap to click), Direct \
         pointer jumps to your finger, Touch passthrough sends real touches.",
    );
    let shortcuts_toggle = setting_toggle(
        ctx,
        "Capture system shortcuts (Alt+Tab, Win, \u{2026})",
        s.inhibit_shortcuts,
        |s, on| s.inhibit_shortcuts = on,
    )
    .tooltip("Off: Alt+Tab, Win & co. act on this machine while the stream input is captured.");

    // --- Audio -----------------------------------------------------------------------------
    let (ac_names, ac_i) = presets(AUDIO_CHANNELS, |v| *v == s.audio_channels);
    let channels_combo = setting_combo(ctx, "Audio channels", ac_names, ac_i, |s, i| {
        s.audio_channels = AUDIO_CHANNELS[i].0;
    })
    .tooltip("The host downmixes if its output has fewer channels.");
    let mic_toggle = setting_toggle(
        ctx,
        "Stream microphone to the host",
        s.mic_enabled,
        |s, on| s.mic_enabled = on,
    )
    .tooltip("Sends the default microphone to the host's virtual mic source.");

    let (hud_names, hud_i) = presets(STATS_TIERS, |v| *v == s.stats_verbosity());
    let hud_combo = setting_combo(ctx, "Stats overlay (HUD)", hud_names, hud_i, |s, i| {
        s.set_stats_verbosity(STATS_TIERS[i].0);
    })
    .tooltip(
        "How much the in-stream overlay shows: Compact (fps \u{00B7} latency \u{00B7} bitrate \
         in one line) \u{2192} Normal \u{2192} Detailed (decode path and per-stage latency). \
         Ctrl+Alt+Shift+S cycles the tiers live while streaming.",
    );

    let licenses_button = {
        let ss = set_screen.clone();
        button("Third-party licenses").on_click(move || ss.call(Screen::Licenses))
    };
    let library_toggle = setting_toggle(
        ctx,
        "Show game library (experimental)",
        s.library_enabled,
        |s, on| s.library_enabled = on,
    )
    .tooltip(
        "Adds \u{201C}Browse library\u{2026}\u{201D} to paired hosts \u{2014} pick a game and it \
         launches in the stream. Mirrors the Apple client's toggle.",
    );
    // App identity + version at the top of the About card (the WinUI Settings convention; the About
    // screen previously showed no version at all). CARGO_PKG_VERSION is the workspace version, baked
    // in at compile time.
    let about_identity = vstack((
        text_block("Punktfunk").font_size(20.0).semibold(),
        text_block(concat!("Version ", env!("CARGO_PKG_VERSION")))
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText),
    ))
    .spacing(2.0);

    // The selected section's content — per-control guidance lives on hover tooltips, so the
    // card is just the controls.
    let (title, card): (&str, Element) = match section {
        "video" => (
            "Video",
            settings_card({
                let mut controls: Vec<Element> = vec![decoder_combo.into()];
                if let Some(c) = gpu_combo {
                    controls.push(c.into());
                }
                controls.extend([
                    codec_combo.into(),
                    bitrate_box.into(),
                    hdr_toggle.into(),
                    hud_combo.into(),
                ]);
                controls
            }),
        ),
        "input" => (
            "Input",
            settings_card(vec![
                forward_combo.into(),
                pad_combo.into(),
                touch_combo.into(),
                shortcuts_toggle.into(),
            ]),
        ),
        "audio" => (
            "Audio",
            settings_card(vec![channels_combo.into(), mic_toggle.into()]),
        ),
        "about" => (
            "About",
            settings_card(vec![
                about_identity.into(),
                library_toggle.into(),
                licenses_button.into(),
            ]),
        ),
        _ => (
            "Display",
            settings_card(vec![
                res_combo.into(),
                hz_combo.into(),
                scale_combo.into(),
                fullscreen_toggle.into(),
                comp_combo.into(),
            ]),
        ),
    };

    // The stock WinUI sidebar (Windows-Settings pattern): pane on the left, the section's card
    // as content, the NavigationView's own back arrow returning to the host list. Auto display
    // mode collapses the pane on a narrow window, exactly like Windows Settings.
    let items = vec![
        NavViewItem::new("Display")
            .tag("display")
            .icon(Symbol::FullScreen),
        NavViewItem::new("Video").tag("video").icon(Symbol::Video),
        NavViewItem::new("Input")
            .tag("input")
            .icon(Symbol::Keyboard),
        NavViewItem::new("Audio").tag("audio").icon(Symbol::Volume),
        NavViewItem::new("About").tag("about").icon(Symbol::Help),
    ];
    // The card is KEYED by section so switching panes REMOUNTS it instead of diffing one
    // section's controls into another's: an in-place diff re-sets a reused ComboBox's items
    // (which clears WinUI's selection) but skips `selected_index` whenever the two sections'
    // values compare equal — the combo then renders with no selected option. A fresh mount
    // applies every prop, so the selection always displays.
    //
    // The content column (not the NavigationView — the sidebar must stay put) carries the
    // section-switch entrance: fade + slide-up from the root-driven tween.
    let content = page_wide(vec![card.with_key(section)])
        .opacity(progress)
        .margin(edges(0.0, (1.0 - progress) * 22.0, 0.0, 0.0));
    NavigationView::new(items, content)
        .pane_title("Settings")
        .header(title)
        .selected_tag(section)
        .on_selection_changed({
            let ss = set_section.clone();
            move |tag: String| ss.call(tag)
        })
        .settings_visible(false)
        .back_enabled(true)
        .on_back_requested({
            let ss = set_screen.clone();
            move || ss.call(Screen::Hosts)
        })
        .into()
}
