//! The settings screen. Every control writes straight back to the persisted [`Settings`]
//! (there is no Apply step), via the small [`setting_combo`]/[`setting_toggle`] builders.

use super::style::*;
use super::{AppCtx, Screen};
use crate::trust::Settings;
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
/// Decode backend presets: `(stored value, display label)`.
const DECODERS: &[(&str, &str)] = &[
    ("auto", "Automatic (GPU, fall back to CPU)"),
    ("hardware", "Hardware (GPU / D3D11VA)"),
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
        .on_changed(move |v: bool| {
            let mut s = ctx.settings.lock().unwrap();
            apply(&mut s, v);
            s.save();
        })
}

/// A titled settings card: bold heading, a secondary description, then the controls.
fn settings_card(title: &str, blurb: &str, controls: Vec<Element>) -> Element {
    let mut children: Vec<Element> = vec![
        text_block(title).font_size(15.0).semibold().into(),
        text_block(blurb)
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText)
            .into(),
    ];
    children.extend(controls);
    card(vstack(children).spacing(10.0)).into()
}

pub(crate) fn settings_page(ctx: &Arc<AppCtx>, set_screen: &AsyncSetState<Screen>) -> Element {
    let s = ctx.settings.lock().unwrap().clone();

    // --- Display ---------------------------------------------------------------------------
    let (res_names, res_i) = {
        let names: Vec<String> = RESOLUTIONS
            .iter()
            .map(|&(w, h)| {
                if w == 0 {
                    "Native display".into()
                } else {
                    format!("{w} \u{00D7} {h}")
                }
            })
            .collect();
        let i = RESOLUTIONS
            .iter()
            .position(|&(w, h)| w == s.width && h == s.height)
            .unwrap_or(0);
        (names, i)
    };
    let res_combo = setting_combo(ctx, "Resolution", res_names, res_i, |s, i| {
        (s.width, s.height) = RESOLUTIONS[i];
    });
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
    });
    let (comp_names, comp_i) = presets(COMPOSITORS, |v| *v == s.compositor);
    let comp_combo = setting_combo(ctx, "Host compositor", comp_names, comp_i, |s, i| {
        s.compositor = COMPOSITORS[i].0.to_string();
    });

    // --- Video -----------------------------------------------------------------------------
    let (dec_names, dec_i) = presets(DECODERS, |v| *v == s.decoder);
    let decoder_combo = setting_combo(ctx, "Video decoder", dec_names, dec_i, |s, i| {
        s.decoder = DECODERS[i].0.to_string();
    });
    let (codec_names, codec_i) = presets(CODECS, |v| *v == s.codec);
    let codec_combo = setting_combo(ctx, "Video codec", codec_names, codec_i, |s, i| {
        s.codec = CODECS[i].0.to_string();
    });
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
    };
    let hdr_toggle = setting_toggle(ctx, "HDR (10-bit, BT.2020 PQ)", s.hdr_enabled, |s, on| {
        s.hdr_enabled = on
    });

    // --- Input -----------------------------------------------------------------------------
    // Which physical controller forwards as pad 0: automatic = the most recently connected;
    // pinning survives until the app exits (Swift/GTK parity).
    let pads = ctx.gamepad.pads();
    let (fwd_names, fwd_i) = {
        let mut names = vec!["Automatic (most recent)".to_string()];
        names.extend(pads.iter().map(|p| {
            let kind = p.kind_label();
            if kind.is_empty() {
                p.name.clone()
            } else {
                format!("{} \u{00B7} {kind}", p.name)
            }
        }));
        let i = ctx
            .gamepad
            .pinned()
            .and_then(|id| pads.iter().position(|p| p.id == id))
            .map_or(0, |i| i + 1);
        (names, i)
    };
    let forward_combo = {
        let svc = ctx.gamepad.clone();
        let ids: Vec<u32> = pads.iter().map(|p| p.id).collect();
        ComboBox::new(fwd_names)
            .header("Forwarded controller")
            .selected_index(fwd_i as i32)
            .on_selection_changed(move |i: i32| {
                let sel = i.max(0) as usize;
                svc.set_pinned(if sel == 0 {
                    None
                } else {
                    ids.get(sel - 1).copied()
                });
            })
    };
    let (pad_names, pad_i) = presets(GAMEPADS, |v| {
        GamepadPref::from_name(v) == GamepadPref::from_name(&s.gamepad)
    });
    let pad_combo = setting_combo(ctx, "Gamepad type", pad_names, pad_i, |s, i| {
        s.gamepad = GAMEPADS[i].0.to_string();
    });
    let shortcuts_toggle = setting_toggle(
        ctx,
        "Capture system shortcuts (Alt+Tab, Win, \u{2026})",
        s.inhibit_shortcuts,
        |s, on| s.inhibit_shortcuts = on,
    );

    // --- Audio -----------------------------------------------------------------------------
    let (ac_names, ac_i) = presets(AUDIO_CHANNELS, |v| *v == s.audio_channels);
    let channels_combo = setting_combo(ctx, "Audio channels", ac_names, ac_i, |s, i| {
        s.audio_channels = AUDIO_CHANNELS[i].0;
    });
    let mic_toggle = setting_toggle(
        ctx,
        "Stream microphone to the host",
        s.mic_enabled,
        |s, on| s.mic_enabled = on,
    );

    let back_btn = button("Back").accent().icon(SymbolGlyph::Back).on_click({
        let ss = set_screen.clone();
        move || ss.call(Screen::Hosts)
    });
    let licenses_button = {
        let ss = set_screen.clone();
        button("Third-party licenses").on_click(move || ss.call(Screen::Licenses))
    };

    page(vec![
        page_header("Settings", back_btn),
        section("DISPLAY"),
        settings_card(
            "Display",
            "The host creates a virtual display at exactly this mode. The compositor choice is \
             advisory (Linux hosts only).",
            vec![res_combo.into(), hz_combo.into(), comp_combo.into()],
        ),
        section("VIDEO"),
        settings_card(
            "Video",
            "Hardware decode (D3D11VA) is zero-copy and far lighter than software — keep it on \
             Automatic unless debugging. Run a per-host speed test (host list) before setting a \
             high bitrate.",
            vec![
                decoder_combo.into(),
                codec_combo.into(),
                bitrate_box.into(),
                hdr_toggle.into(),
            ],
        ),
        section("INPUT"),
        settings_card(
            "Input",
            "Exactly one controller is forwarded to the host; \u{201C}Automatic\u{201D} picks the \
             most recently connected. The gamepad type is the virtual pad the host creates.",
            vec![
                forward_combo.into(),
                pad_combo.into(),
                shortcuts_toggle.into(),
            ],
        ),
        section("AUDIO"),
        settings_card(
            "Audio",
            "Request stereo or surround — the host downmixes if its output has fewer.",
            vec![channels_combo.into(), mic_toggle.into()],
        ),
        section("ABOUT"),
        settings_card(
            "About",
            "punktfunk is licensed under MIT OR Apache-2.0.",
            vec![licenses_button.into()],
        ),
    ])
}
