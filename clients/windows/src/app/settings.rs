//! The settings screen. Every control writes straight back to the persisted [`Settings`]
//! (there is no Apply step), via the small [`setting_combo`]/[`setting_toggle`] builders.
//!
//! **Structure mirrors the Apple client's 2026-07 settings revamp** (its
//! `SettingsCategory` + `SettingsView+Sections.swift`), so the two desktop clients read the
//! same way: General = session/app behavior, Display = everything about the picture,
//! Input = touch/keyboard/mouse, Audio, Controllers, About. Each field carries its
//! explanation DIRECTLY under it ([`described`]) rather than only on hover — the same move
//! Apple made, for the same reason (guidance nobody hovers for is guidance nobody reads).
//! Wording is shared verbatim wherever the setting means the same thing on both platforms;
//! where the BEHAVIOR differs the text is deliberately Windows-specific (the forwarded-
//! controller picker especially: Apple forwards one pad, this client forwards them all).

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
    ("vulkan", "Hardware (Vulkan Video)"),
    ("d3d11va", "Hardware (Direct3D 11 / DXVA)"),
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
    // Preference-only by design: `resolve_codec` never auto-picks PyroWave, and asking for
    // it on a host or device that can't do it simply falls back down the ladder to HEVC.
    ("pyrowave", "PyroWave (wired LAN)"),
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

/// One field: the control with its explanation directly underneath (Apple's `described`).
///
/// The caption goes BELOW the control on purpose. An earlier revision put guidance only in
/// hover tooltips because a paragraph *above* a control reads as that control's label — true,
/// but a caption under it reads as a caption, which is how every Windows Settings page and
/// the Apple client both do it. Width-capped for the same reason Apple caps at 360pt: a
/// full-width caption runs into the control column and the whole cell reads as one block.
fn described(control: impl Into<Element>, caption: &str) -> Element {
    vstack((
        control.into(),
        text_block(caption)
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText)
            .wrap()
            .max_width(420.0)
            // Stretch (the TextBlock default) CENTRES a MaxWidth-capped block in the leftover
            // width — the caption must be pinned left or it drifts away from its control.
            .horizontal_alignment(HorizontalAlignment::Left),
    ))
    .spacing(5.0)
    .into()
}

/// One settings group: an optional sub-section label, a card of fields, and an optional
/// form-level note under it (Apple's Section header/footer). Groups stack down the page.
fn group(header: Option<&str>, fields: Vec<Element>, footer: Option<&str>) -> Vec<Element> {
    let mut out = Vec::with_capacity(3);
    if let Some(h) = header {
        out.push(section(h));
    }
    out.push(card(vstack(fields).spacing(14.0)).into());
    if let Some(f) = footer {
        out.push(
            text_block(f)
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText)
                .wrap()
                .horizontal_alignment(HorizontalAlignment::Left)
                .margin(edges(2.0, 6.0, 0.0, 0.0))
                .into(),
        );
    }
    out
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
    });
    let (comp_names, comp_i) = presets(COMPOSITORS, |v| *v == s.compositor);
    let comp_combo = setting_combo(ctx, "Host compositor", comp_names, comp_i, |s, i| {
        s.compositor = COMPOSITORS[i].0.to_string();
    });
    let auto_wake_toggle = setting_toggle(ctx, "Auto-wake on connect", s.auto_wake, |s, on| {
        s.auto_wake = on
    });
    let fullscreen_toggle = setting_toggle(
        ctx,
        "Start streams fullscreen",
        s.fullscreen_on_stream,
        |s, on| s.fullscreen_on_stream = on,
    );

    // --- Video -----------------------------------------------------------------------------
    let (dec_names, dec_i) = presets(DECODERS, |v| *v == s.decoder);
    let decoder_combo = setting_combo(ctx, "Video decoder", dec_names, dec_i, |s, i| {
        s.decoder = DECODERS[i].0.to_string();
    });
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
    };
    let (pad_names, pad_i) = presets(GAMEPADS, |v| {
        GamepadPref::from_name(v) == GamepadPref::from_name(&s.gamepad)
    });
    let pad_combo = setting_combo(ctx, "Gamepad type", pad_names, pad_i, |s, i| {
        s.gamepad = GAMEPADS[i].0.to_string();
    });
    let (touch_names, touch_i) = presets(TOUCH_MODES, |v| *v == s.touch_mode);
    let touch_combo = setting_combo(ctx, "Touch input", touch_names, touch_i, |s, i| {
        s.touch_mode = TOUCH_MODES[i].0.to_string();
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

    let (hud_names, hud_i) = presets(STATS_TIERS, |v| *v == s.stats_verbosity());
    let hud_combo = setting_combo(ctx, "Stats overlay (HUD)", hud_names, hud_i, |s, i| {
        s.set_stats_verbosity(STATS_TIERS[i].0);
    });

    let licenses_button = {
        let ss = set_screen.clone();
        button("Third-party licenses").on_click(move || ss.call(Screen::Licenses))
    };
    let library_toggle = setting_toggle(
        ctx,
        "Show game library (experimental)",
        s.library_enabled,
        |s, on| s.library_enabled = on,
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

    // The selected section's content, grouped exactly like the Apple client's categories
    // (SettingsCategory + SettingsView+Sections.swift). Each field's explanation sits under
    // it; the only form-level notes are the "applies from the next session" footers, matching
    // Apple's decision to keep exactly one of those per affected category.
    let (title, groups): (&str, Vec<Element>) = match section {
        "display" => {
            let mut out = group(
                Some("Resolution"),
                vec![
                    described(
                        res_combo,
                        "The host drives a real virtual output at exactly this size \u{2014} true \
                         pixels, no scaling. \u{201C}Native display\u{201D} follows the monitor this \
                         window is on; \u{201C}Match window\u{201D} keeps the picture pixel-exact \
                         (1:1) through every resize.",
                    ),
                    described(
                        hz_combo,
                        "\u{201C}Native\u{201D} resolves to this display\u{2019}s refresh rate at \
                         connect.",
                    ),
                ],
                None,
            );
            out.extend(group(
                Some("Quality"),
                vec![
                    described(
                        scale_combo,
                        "Above native supersamples for sharpness; below renders lighter on the \
                         host and the link. This device resamples the result to the window.",
                    ),
                    described(
                        bitrate_box,
                        "0 lets the host decide (its default, clamped to what it supports). A \
                         host card\u{2019}s context menu has a network speed test.",
                    ),
                    described(
                        codec_combo,
                        "A preference \u{2014} the host falls back if it can\u{2019}t encode it. \
                         PyroWave is the low-latency wavelet codec for a WIRED link: it trades \
                         bitrate (hundreds of Mb/s) for near-zero decode time, so it wants \
                         gigabit Ethernet.",
                    ),
                    described(
                        hdr_toggle,
                        "HDR10, when the host has HDR content and this display supports it. \
                         HEVC only; otherwise the stream stays SDR.",
                    ),
                ],
                None,
            ));
            out.extend(group(
                Some("Decoding"),
                {
                    let mut fields = vec![described(
                        decoder_combo,
                        "Automatic picks the hardware path this GPU does best \u{2014} Direct3D \
                         11 on Intel, Vulkan Video on NVIDIA and AMD \u{2014} and falls back to \
                         the CPU. Change it only when debugging.",
                    )];
                    if let Some(c) = gpu_combo {
                        fields.push(described(
                            c,
                            "Which adapter decodes and presents the stream. Automatic uses the \
                             GPU driving this window\u{2019}s display.",
                        ));
                    }
                    fields
                },
                None,
            ));
            out.extend(group(
                Some("Host output"),
                vec![described(
                    comp_combo,
                    "The backend the host uses for its virtual output (Linux hosts only). A \
                     specific choice falls back to auto-detection when that backend \
                     isn\u{2019}t available.",
                )],
                // The one form-level note, exactly as on Apple.
                Some("Display changes apply from the next session."),
            ));
            ("Display", out)
        }
        "input" => {
            let mut out = group(
                Some("Touch & pointer"),
                vec![described(
                    touch_combo,
                    "How a touchscreen drives the host: Trackpad moves the host cursor like a \
                     laptop trackpad (tap to click), Direct pointer jumps the cursor to wherever \
                     you touch, Touch passthrough sends real multi-touch through.",
                )],
                None,
            );
            out.extend(group(
                Some("Keyboard & mouse"),
                vec![described(
                    shortcuts_toggle,
                    "Alt+Tab, the Windows key and friends reach the host while the stream has \
                     input captured. Off, they act on this machine instead.",
                )],
                None,
            ));
            ("Input", out)
        }
        "controllers" => (
            "Controllers",
            group(
                None,
                vec![
                    // NOT Apple's wording: Apple forwards ONE pad as player 1, this client
                    // forwards every controller as its own player. Same picker, different rule.
                    described(
                        forward_combo,
                        "Every connected controller is forwarded, each as its own player. Pick \
                         one to force single-player \u{2014} only it reaches the host.",
                    ),
                    described(
                        pad_combo,
                        "The virtual pad created on the host. Automatic matches your controller \
                         \u{2014} a DualSense keeps adaptive triggers, lightbar, touchpad and \
                         motion.",
                    ),
                ],
                Some("Applies from the next session."),
            ),
        ),
        "audio" => (
            "Audio",
            group(
                None,
                vec![
                    described(
                        channels_combo,
                        "The speaker layout requested from the host. It downmixes if its own \
                         output has fewer channels.",
                    ),
                    described(
                        mic_toggle,
                        "This device\u{2019}s microphone feeds the host\u{2019}s virtual mic.",
                    ),
                ],
                Some("Applies from the next session."),
            ),
        ),
        "about" => (
            "About",
            group(
                None,
                vec![about_identity.into(), licenses_button.into()],
                None,
            ),
        ),
        // "general" and anything unrecognized.
        _ => {
            let mut out = group(
                Some("Session"),
                vec![
                    described(
                        fullscreen_toggle,
                        "Go fullscreen when a session starts; F11 or Alt+Enter switches back \
                         live.",
                    ),
                    described(
                        auto_wake_toggle,
                        "Connecting to a saved host that\u{2019}s offline sends Wake-on-LAN and \
                         waits for it to boot. Turn off if hosts behind a VPN look offline when \
                         they aren\u{2019}t.",
                    ),
                ],
                None,
            );
            out.extend(group(
                Some("Statistics"),
                vec![described(
                    hud_combo,
                    "Live session stats in a corner overlay \u{2014} Compact is a one-line pill, \
                     Detailed adds the latency stage breakdown. Ctrl+Alt+Shift+S cycles the \
                     tiers any time.",
                )],
                None,
            ));
            out.extend(group(
                Some("Library"),
                vec![described(
                    library_toggle,
                    "Adds \u{201C}Browse library\u{2026}\u{201D} to paired hosts \u{2014} list \
                     their Steam and custom games and launch one directly. No extra host setup.",
                )],
                None,
            ));
            ("General", out)
        }
    };

    // The stock WinUI sidebar (Windows-Settings pattern): pane on the left, the section's card
    // as content, the NavigationView's own back arrow returning to the host list. Auto display
    // mode collapses the pane on a narrow window, exactly like Windows Settings.
    // Category order mirrors the Apple client's sidebar exactly.
    let items = vec![
        NavViewItem::new("General")
            .tag("general")
            .icon(Symbol::Setting),
        NavViewItem::new("Display")
            .tag("display")
            .icon(Symbol::FullScreen),
        NavViewItem::new("Input")
            .tag("input")
            .icon(Symbol::Keyboard),
        NavViewItem::new("Audio").tag("audio").icon(Symbol::Volume),
        NavViewItem::new("Controllers")
            .tag("controllers")
            .icon(Symbol::Play),
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
    // No max-width cap here (unlike the other pages): the NavigationView already spends the
    // left third on its pane, so a 640-wide column left the cards as a narrow ribbon.
    let content = scroll_view(
        vstack(groups)
            .spacing(10.0)
            .margin(edges(24.0, 20.0, 28.0, 40.0))
            .with_key(section),
    )
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
