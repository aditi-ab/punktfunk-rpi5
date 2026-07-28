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
use crate::trust::{KnownHosts, Settings};
use pf_client_core::profiles::{ProfilesFile, StreamProfile};
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
/// Physical-mouse presets: `(stored value, display label)` — capture (pointer lock,
/// relative, for games) vs desktop (uncaptured absolute pointer, for remote desktop
/// work). Ctrl+Alt+Shift+M flips the model live in-stream.
const MOUSE_MODES: &[(&str, &str)] = &[
    ("capture", "Capture (games)"),
    ("desktop", "Desktop (absolute)"),
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
/// The sentinel the scope ComboBox's last entry carries — "New profile…" is an action, not a
/// layer, and a real id can never collide with it (ids are 12 hex chars).
const NEW_PROFILE: &str = "\u{1}new";

/// Rename / duplicate / delete for the profile in scope. Rename is a text box rather than a
/// modal because this toolkit's ContentDialog is text-only (the same constraint that put the
/// host editor in a tile); it commits on change, which is how every other control here works.
fn profile_actions(
    profile: &StreamProfile,
    set_scope: &AsyncSetState<String>,
    set_delete: &AsyncSetState<Option<String>>,
) -> Element {
    let id = profile.id.clone();
    let name_box = {
        let id = id.clone();
        text_box(&profile.name)
            .placeholder_text("Profile name")
            .on_text_changed(move |t: String| {
                let name = t.trim().to_string();
                if name.is_empty() {
                    return;
                }
                let mut catalog = ProfilesFile::load();
                // Names are unique case-insensitively — menus keyed by name are ambiguous
                // otherwise. A collision simply doesn't commit; the box keeps what was typed.
                if catalog.name_taken(&name, Some(&id)) {
                    return;
                }
                if let Some(p) = catalog.profiles.iter_mut().find(|p| p.id == id) {
                    p.name = name;
                    let _ = catalog.save();
                }
            })
    };
    let duplicate = {
        let (id, set_scope) = (id.clone(), set_scope.clone());
        button("Duplicate").on_click(move || {
            let mut catalog = ProfilesFile::load();
            let Some(source) = catalog.find_by_id(&id).cloned() else {
                return;
            };
            let name = (2..)
                .map(|n| format!("{} {n}", source.name))
                .find(|n| !catalog.name_taken(n, None))
                .unwrap_or_else(|| source.name.clone());
            let mut copy = StreamProfile::new(name);
            copy.overrides = source.overrides.clone();
            copy.accent = source.accent.clone();
            let new_id = copy.id.clone();
            catalog.profiles.push(copy);
            if catalog.save().is_ok() {
                set_scope.call(new_id);
            }
        })
    };
    let delete = {
        let (id, set_delete) = (id.clone(), set_delete.clone());
        button("Delete\u{2026}").on_click(move || set_delete.call(Some(id.clone())))
    };
    described(
        vstack((name_box, hstack((duplicate, delete)).spacing(8.0))).spacing(8.0),
        "Renaming applies as you type. Deleting leaves hosts that used it on Default settings.",
    )
}

/// Persist one control's edit into the layer being edited.
///
/// This shell commits PER CONTROL (unlike the GTK one, which writes when its dialog closes),
/// so it can't hand the profile a list of touched fields. It hands over the effective settings
/// before and after instead, and [`SettingsOverlay::absorb`] records the field that moved —
/// the comparison is against what the control was SHOWING, so picking a value that happens to
/// equal the global still records an override (the pin the design asks for).
fn commit(ctx: &Arc<AppCtx>, scope: &str, edit: impl FnOnce(&mut Settings)) {
    if scope.is_empty() {
        let mut s = ctx.settings.lock().unwrap();
        edit(&mut s);
        s.save();
        return;
    }
    let mut catalog = ProfilesFile::load();
    let base = ctx.settings.lock().unwrap().clone();
    let Some(p) = catalog.profiles.iter_mut().find(|p| p.id == scope) else {
        return; // deleted from under us; the next render falls back to the defaults scope
    };
    let before = p.overrides.apply(&base);
    let mut after = before.clone();
    edit(&mut after);
    p.overrides.absorb(&before, &after);
    if let Err(e) = catalog.save() {
        tracing::warn!(error = %format!("{e:#}"), "saving the profile catalog");
    }
}

/// Which tier-P rows the profile in scope overrides. Plain bools rather than a lookup so the
/// call sites read as `over.codec` — the row and its flag stay visibly paired.
#[derive(Default)]
struct OverrideFlags {
    resolution: bool,
    refresh_hz: bool,
    render_scale: bool,
    bitrate_kbps: bool,
    codec: bool,
    hdr_enabled: bool,
    compositor: bool,
    audio_channels: bool,
    mic_enabled: bool,
    touch_mode: bool,
    mouse_mode: bool,
    invert_scroll: bool,
    inhibit_shortcuts: bool,
    gamepad: bool,
    stats_verbosity: bool,
    fullscreen_on_stream: bool,
}

impl OverrideFlags {
    fn of(profile: Option<&StreamProfile>) -> OverrideFlags {
        let Some(o) = profile.map(|p| &p.overrides) else {
            return OverrideFlags::default();
        };
        OverrideFlags {
            // One control drives the width/height/match-window tri-state, so any of the three
            // marks the row.
            resolution: o.width.is_some() || o.height.is_some() || o.match_window.is_some(),
            refresh_hz: o.refresh_hz.is_some(),
            render_scale: o.render_scale.is_some(),
            bitrate_kbps: o.bitrate_kbps.is_some(),
            codec: o.codec.is_some(),
            hdr_enabled: o.hdr_enabled.is_some(),
            compositor: o.compositor.is_some(),
            audio_channels: o.audio_channels.is_some(),
            mic_enabled: o.mic_enabled.is_some(),
            touch_mode: o.touch_mode.is_some(),
            mouse_mode: o.mouse_mode.is_some(),
            invert_scroll: o.invert_scroll.is_some(),
            inhibit_shortcuts: o.inhibit_shortcuts.is_some(),
            gamepad: o.gamepad.is_some(),
            stats_verbosity: o.stats_verbosity.is_some(),
            fullscreen_on_stream: o.fullscreen_on_stream.is_some(),
        }
    }
}

/// The layer the settings screen is editing, resolved for display: `None` = the defaults.
fn active_profile(scope: &str) -> Option<StreamProfile> {
    (!scope.is_empty())
        .then(|| ProfilesFile::load().find_by_id(scope).cloned())
        .flatten()
}

fn setting_combo(
    ctx: &Arc<AppCtx>,
    scope: &str,
    header: &str,
    names: Vec<String>,
    current: usize,
    apply: impl Fn(&mut Settings, usize) + 'static,
) -> ComboBox {
    let (ctx, scope) = (ctx.clone(), scope.to_string());
    let max = names.len().saturating_sub(1);
    ComboBox::new(names)
        .header(header)
        .selected_index(current as i32)
        .on_selection_changed(move |i: i32| {
            commit(&ctx, &scope, |s| apply(s, (i.max(0) as usize).min(max)));
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
    scope: &str,
    header: &str,
    on: bool,
    apply: impl Fn(&mut Settings, bool) + 'static,
) -> ToggleSwitch {
    let (ctx, scope) = (ctx.clone(), scope.to_string());
    ToggleSwitch::new(on)
        .header(header)
        .on_content("On")
        .off_content("Off")
        .on_toggled(move |v: bool| {
            commit(&ctx, &scope, |s| apply(s, v));
        })
}

/// One field: the control with its explanation directly underneath (Apple's `described`).
///
/// The caption goes BELOW the control on purpose. An earlier revision put guidance only in
/// hover tooltips because a paragraph *above* a control reads as that control's label — true,
/// but a caption under it reads as a caption, which is how every Windows Settings page and
/// the Apple client both do it. Width-capped for the same reason Apple caps at 360pt: a
/// full-width caption runs into the control column and the whole cell reads as one block.
/// [`described`], plus the override marker and reset a profile-scope row carries: the caption
/// says the profile changes this one, and the button is the only way back to inheriting —
/// overrides are recorded on touch and never inferred from a value comparison, so "not
/// overridden" needs an explicit act.
fn described_overridable(
    rev: (u64, &AsyncSetState<u64>),
    scope: &str,
    field: &'static str,
    overridden: bool,
    control: impl Into<Element>,
    caption: &str,
) -> Element {
    if scope.is_empty() || !overridden {
        return described(control, caption);
    }
    let (rev, set_rev) = (rev.0, rev.1.clone());
    let scope = scope.to_string();
    vstack((
        control.into(),
        hstack((
            text_block(format!("\u{25cf} Overridden here \u{00b7} {caption}"))
                .font_size(12.0)
                .foreground(ThemeRef::AccentText)
                .wrap()
                .max_width(360.0)
                .horizontal_alignment(HorizontalAlignment::Left),
            button("Reset").on_click(move || {
                let mut catalog = ProfilesFile::load();
                if let Some(p) = catalog.profiles.iter_mut().find(|p| p.id == scope) {
                    p.overrides.clear(field);
                    if let Err(e) = catalog.save() {
                        tracing::warn!(error = %format!("{e:#}"), "clearing an override");
                    }
                }
                // The catalog changed behind the controls, and nothing the page reads as
                // state did — bump the revision so the row re-renders showing the inherited
                // value again.
                set_rev.call(rev + 1);
            }),
        ))
        .spacing(8.0),
    ))
    .spacing(5.0)
    .into()
}

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

/// A settings sub-section heading. Deliberately NOT the shared [`section`] helper: that one
/// carries a 2px left inset (fine over the hosts/licenses lists it was written for), which
/// here left every heading hanging one nudge right of the card edge below it. Flush left, so
/// heading and card share one line.
fn group_heading(label: &str) -> Element {
    text_block(label)
        .font_size(12.0)
        .semibold()
        .foreground(ThemeRef::SecondaryText)
        .horizontal_alignment(HorizontalAlignment::Left)
        .margin(edges(0.0, 14.0, 0.0, 2.0))
        .into()
}

/// One settings group: an optional sub-section label, a card of fields, and an optional
/// form-level note under it (Apple's Section header/footer). Groups stack down the page.
fn group(header: Option<&str>, fields: Vec<Element>, footer: Option<&str>) -> Vec<Element> {
    let mut out = Vec::with_capacity(3);
    if let Some(h) = header {
        out.push(group_heading(h));
    }
    out.push(card(vstack(fields).spacing(14.0)).into());
    if let Some(f) = footer {
        out.push(
            text_block(f)
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText)
                .wrap()
                .horizontal_alignment(HorizontalAlignment::Left)
                .margin(edges(0.0, 6.0, 0.0, 0.0))
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn settings_page(
    ctx: &Arc<AppCtx>,
    set_screen: &AsyncSetState<Screen>,
    section: &str,
    set_section: &AsyncSetState<String>,
    scope_id: &str,
    set_scope: &AsyncSetState<String>,
    delete_pending: &Option<String>,
    set_delete: &AsyncSetState<Option<String>>,
    rev: u64,
    set_rev: &AsyncSetState<u64>,
    progress: f64,
) -> Element {
    // The layer being edited. A scope pointing at a deleted profile degrades to the defaults,
    // the same rule a dangling host binding follows.
    let active = active_profile(scope_id);
    let scope: &str = match &active {
        Some(p) => &p.id,
        None => "",
    };
    let profile_mode = active.is_some();
    // Which rows this profile overrides — the marker + reset each of them carries. In the
    // defaults scope nothing is marked, and `described_overridable` degrades to `described`.
    let over = OverrideFlags::of(active.as_ref());
    let _ = rev; // read via the closures below; the value itself only forces the re-render
                 // Every control shows the EFFECTIVE value: the global underneath with this profile's
                 // overrides on top, so a row the profile doesn't override reads as the live global.
    let s = {
        let base = ctx.settings.lock().unwrap().clone();
        match &active {
            Some(p) => p.overrides.apply(&base),
            None => base,
        }
    };

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
    let res_combo = setting_combo(ctx, scope, "Resolution", res_names, res_i, |s, i| {
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
    let hz_combo = setting_combo(ctx, scope, "Refresh rate", hz_names, hz_i, |s, i| {
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
    let scale_combo = setting_combo(ctx, scope, "Render scale", scale_names, scale_i, |s, i| {
        s.render_scale = RENDER_SCALES[i];
    });
    let (comp_names, comp_i) = presets(COMPOSITORS, |v| *v == s.compositor);
    let comp_combo = setting_combo(ctx, scope, "Host compositor", comp_names, comp_i, |s, i| {
        s.compositor = COMPOSITORS[i].0.to_string();
    });
    let auto_wake_toggle =
        setting_toggle(ctx, scope, "Auto-wake on connect", s.auto_wake, |s, on| {
            s.auto_wake = on
        });
    let fullscreen_toggle = setting_toggle(
        ctx,
        scope,
        "Start streams fullscreen",
        s.fullscreen_on_stream,
        |s, on| s.fullscreen_on_stream = on,
    );

    // --- Video -----------------------------------------------------------------------------
    let (dec_names, dec_i) = presets(DECODERS, |v| *v == s.decoder);
    let decoder_combo = setting_combo(ctx, scope, "Video decoder", dec_names, dec_i, |s, i| {
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
        setting_combo(ctx, scope, "GPU", names, current, move |s, i| {
            s.adapter = if i == 0 {
                String::new()
            } else {
                gpus[i - 1].clone()
            };
        })
    });
    let (codec_names, codec_i) = presets(CODECS, |v| *v == s.codec);
    let codec_combo = setting_combo(ctx, scope, "Video codec", codec_names, codec_i, |s, i| {
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
    let hdr_toggle = setting_toggle(
        ctx,
        scope,
        "HDR (10-bit, BT.2020 PQ)",
        s.hdr_enabled,
        |s, on| s.hdr_enabled = on,
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
    };
    let (pad_names, pad_i) = presets(GAMEPADS, |v| {
        GamepadPref::from_name(v) == GamepadPref::from_name(&s.gamepad)
    });
    let pad_combo = setting_combo(ctx, scope, "Gamepad type", pad_names, pad_i, |s, i| {
        s.gamepad = GAMEPADS[i].0.to_string();
    });
    let (touch_names, touch_i) = presets(TOUCH_MODES, |v| *v == s.touch_mode);
    let touch_combo = setting_combo(ctx, scope, "Touch input", touch_names, touch_i, |s, i| {
        s.touch_mode = TOUCH_MODES[i].0.to_string();
    });
    let (mouse_names, mouse_i) = presets(MOUSE_MODES, |v| *v == s.mouse_mode);
    let mouse_combo = setting_combo(ctx, scope, "Mouse input", mouse_names, mouse_i, |s, i| {
        s.mouse_mode = MOUSE_MODES[i].0.to_string();
    });
    let invert_scroll_toggle = setting_toggle(
        ctx,
        scope,
        "Invert scroll direction",
        s.invert_scroll,
        |s, on| s.invert_scroll = on,
    );
    let shortcuts_toggle = setting_toggle(
        ctx,
        scope,
        "Capture system shortcuts (Alt+Tab, Win, \u{2026})",
        s.inhibit_shortcuts,
        |s, on| s.inhibit_shortcuts = on,
    );

    // --- Audio -----------------------------------------------------------------------------
    let (ac_names, ac_i) = presets(AUDIO_CHANNELS, |v| *v == s.audio_channels);
    let channels_combo = setting_combo(ctx, scope, "Audio channels", ac_names, ac_i, |s, i| {
        s.audio_channels = AUDIO_CHANNELS[i].0;
    });
    let mic_toggle = setting_toggle(
        ctx,
        scope,
        "Stream microphone to the host",
        s.mic_enabled,
        |s, on| s.mic_enabled = on,
    );

    let (hud_names, hud_i) = presets(STATS_TIERS, |v| *v == s.stats_verbosity());
    let hud_combo = setting_combo(
        ctx,
        scope,
        "Stats overlay (HUD)",
        hud_names,
        hud_i,
        |s, i| {
            s.set_stats_verbosity(STATS_TIERS[i].0);
        },
    );

    let licenses_button = {
        let ss = set_screen.clone();
        button("Third-party licenses").on_click(move || ss.call(Screen::Licenses))
    };
    let library_toggle = setting_toggle(
        ctx,
        scope,
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
                    described_overridable(
                        (rev, set_rev),
                        scope,
                        "resolution",
                        over.resolution,
                        res_combo,
                        "The host drives a real virtual output at exactly this size \u{2014} true \
                         pixels, no scaling. \u{201C}Native display\u{201D} follows the monitor this \
                         window is on; \u{201C}Match window\u{201D} keeps the picture pixel-exact \
                         (1:1) through every resize.",
                    ),
                    described_overridable(
                        (rev, set_rev),
                        scope,
                        "refresh_hz",
                        over.refresh_hz,
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
                    described_overridable(
                        (rev, set_rev),
                        scope,
                        "render_scale",
                        over.render_scale,
                        scale_combo,
                        "Above native supersamples for sharpness; below renders lighter on the \
                         host and the link. This device resamples the result to the window.",
                    ),
                    described_overridable(
                        (rev, set_rev),
                        scope,
                        "bitrate_kbps",
                        over.bitrate_kbps,
                        bitrate_box,
                        "0 lets the host decide (its default, clamped to what it supports). A \
                         host card\u{2019}s context menu has a network speed test.",
                    ),
                    described_overridable(
                        (rev, set_rev),
                        scope,
                        "codec",
                        over.codec,
                        codec_combo,
                        "A preference \u{2014} the host falls back if it can\u{2019}t encode it. \
                         PyroWave is the low-latency wavelet codec for a WIRED link: it trades \
                         bitrate (hundreds of Mb/s) for near-zero decode time, so it wants \
                         gigabit Ethernet.",
                    ),
                    described_overridable(
                        (rev, set_rev),
                        scope,
                        "hdr_enabled",
                        over.hdr_enabled,
                        hdr_toggle,
                        "HDR10, when the host has HDR content and this display supports it. \
                         HEVC only; otherwise the stream stays SDR.",
                    ),
                ],
                None,
            ));
            // Decoder and GPU are facts about THIS device's hardware — never per profile.
            out.extend(group(
                Some("Decoding"),
                if profile_mode {
                    Vec::new()
                } else {
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
                vec![described_overridable(
                    (rev, set_rev),
                    scope,
                    "compositor",
                    over.compositor,
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
                vec![described_overridable(
                    (rev, set_rev),
                    scope,
                    "touch_mode",
                    over.touch_mode,
                    touch_combo,
                    "How a touchscreen drives the host: Trackpad moves the host cursor like a \
                     laptop trackpad (tap to click), Direct pointer jumps the cursor to wherever \
                     you touch, Touch passthrough sends real multi-touch through.",
                )],
                None,
            );
            out.extend(group(
                Some("Keyboard & mouse"),
                vec![
                    described_overridable(
                        (rev, set_rev),
                        scope,
                        "mouse_mode",
                        over.mouse_mode,
                        mouse_combo,
                        "Capture locks the pointer to the stream and sends relative motion — \
                         best for games. Desktop leaves the pointer free to enter and leave \
                         the stream and sends absolute positions — best for remote desktop \
                         work. Ctrl+Alt+Shift+M switches live.",
                    ),
                    described_overridable(
                        (rev, set_rev),
                        scope,
                        "inhibit_shortcuts",
                        over.inhibit_shortcuts,
                        shortcuts_toggle,
                        "Alt+Tab, the Windows key and friends reach the host while the stream \
                         has input captured. Off, they act on this machine instead.",
                    ),
                    described_overridable(
                        (rev, set_rev),
                        scope,
                        "invert_scroll",
                        over.invert_scroll,
                        invert_scroll_toggle,
                        "Reverses the wheel and trackpad scroll direction sent to the host.",
                    ),
                ],
                None,
            ));
            ("Input", out)
        }
        "controllers" => (
            "Controllers",
            group(
                None,
                [
                    // NOT Apple's wording: Apple forwards ONE pad as player 1, this client
                    // forwards every controller as its own player. Same picker, different rule.
                    // Which physical pad this device forwards is a device fact (tier G), so it
                    // renders only in the defaults scope; the EMULATED type below is profileable.
                    (!profile_mode).then(|| {
                        described(
                        forward_combo,
                        "Every connected controller is forwarded, each as its own player. Pick \
                         one to force single-player \u{2014} only it reaches the host.",
                    )
                    }),
                    Some(described_overridable(
                        (rev, set_rev),
                        scope,
                        "gamepad",
                        over.gamepad,
                        pad_combo,
                        "The virtual pad created on the host. Automatic matches your controller \
                         \u{2014} a DualSense keeps adaptive triggers, lightbar, touchpad and \
                         motion.",
                    )),
                ]
                .into_iter()
                .flatten()
                .collect(),
                Some("Applies from the next session."),
            ),
        ),
        "audio" => (
            "Audio",
            group(
                None,
                vec![
                    described_overridable(
                        (rev, set_rev),
                        scope,
                        "audio_channels",
                        over.audio_channels,
                        channels_combo,
                        "The speaker layout requested from the host. It downmixes if its own \
                         output has fewer channels.",
                    ),
                    described_overridable(
                        (rev, set_rev),
                        scope,
                        "mic_enabled",
                        over.mic_enabled,
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
                vec![described_overridable(
                    (rev, set_rev),
                    scope,
                    "fullscreen_on_stream",
                    over.fullscreen_on_stream,
                    fullscreen_toggle,
                    "Go fullscreen when a session starts; F11 or Alt+Enter switches back \
                         live.",
                )]
                .into_iter()
                // Auto-wake is about this host and this network, not about "Game vs Work" —
                // it stays global in v1 (design §3, tier H/G).
                .chain((!profile_mode).then(|| {
                    described(
                        auto_wake_toggle,
                        "Connecting to a saved host that\u{2019}s offline sends Wake-on-LAN and \
                         waits for it to boot. Turn off if hosts behind a VPN look offline when \
                         they aren\u{2019}t.",
                    )
                }))
                .collect(),
                None,
            );
            out.extend(group(
                Some("Statistics"),
                vec![described_overridable(
                    (rev, set_rev),
                    scope,
                    "stats_verbosity",
                    over.stats_verbosity,
                    hud_combo,
                    "Live session stats in a corner overlay \u{2014} Compact is a one-line pill, \
                     Detailed adds the latency stage breakdown. Ctrl+Alt+Shift+S cycles the \
                     tiers any time.",
                )],
                None,
            ));
            // The library browser is an app-level toggle for this device, not a per-profile one.
            out.extend(group(
                Some("Library"),
                if profile_mode {
                    Vec::new()
                } else {
                    vec![described(
                    library_toggle,
                    "Adds \u{201C}Browse library\u{2026}\u{201D} to paired hosts \u{2014} list \
                     their Steam and custom games and launch one directly. No extra host setup.",
                )]
                },
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
    // The category title is rendered HERE, not via NavigationView's Header: that header's
    // left inset belongs to WinUI's own template (a string prop is all we can set), so it
    // sat noticeably right of the cards under it. In the content column it shares the cards'
    // left edge by construction.
    // The scope switcher sits above the section title, so it reads as "which layer all of
    // this belongs to" rather than as one section's setting.
    let catalog = ProfilesFile::load();
    let mut scope_names = vec!["Default settings".to_string()];
    let mut scope_ids: Vec<String> = vec![String::new()];
    for p in &catalog.profiles {
        scope_names.push(p.name.clone());
        scope_ids.push(p.id.clone());
    }
    scope_names.push("New profile\u{2026}".to_string());
    scope_ids.push(NEW_PROFILE.to_string());
    let scope_i = scope_ids.iter().position(|i| i == scope).unwrap_or(0);
    let switcher = described(
        ComboBox::new(scope_names)
            .header("Editing")
            .selected_index(scope_i as i32)
            .on_selection_changed({
                let (set_scope, ids) = (set_scope.clone(), scope_ids.clone());
                move |i: i32| {
                    let Some(id) = ids.get(i.max(0) as usize) else {
                        return;
                    };
                    if id == NEW_PROFILE {
                        // Creation needs no prompt here: WinUI's ContentDialog is text-only in
                        // this toolkit, so a new profile takes an auto-numbered name and is
                        // renamed from the row below — one fewer modal, same end state.
                        let mut catalog = ProfilesFile::load();
                        let name = (1..)
                            .map(|n| format!("Profile {n}"))
                            .find(|n| !catalog.name_taken(n, None))
                            .unwrap_or_else(|| "Profile".to_string());
                        let profile = StreamProfile::new(name);
                        let new_id = profile.id.clone();
                        catalog.profiles.push(profile);
                        if catalog.save().is_ok() {
                            set_scope.call(new_id);
                        }
                        return;
                    }
                    set_scope.call(id.clone());
                }
            }),
        "A profile overrides only what you change while it is selected; everything else \
         follows Default settings.",
    );
    let mut header_rows = vec![switcher];
    if let Some(p) = &active {
        header_rows.push(profile_actions(p, set_scope, set_delete));
    }
    let titled: Vec<Element> = std::iter::once(
        text_block(title)
            .font_size(28.0)
            .semibold()
            .horizontal_alignment(HorizontalAlignment::Left)
            .margin(edges(0.0, 0.0, 0.0, 6.0))
            .into(),
    )
    .chain(group(None, header_rows, None))
    .chain(groups)
    .collect();
    // The keyed column MUST sit inside a panel's child list, not directly under the
    // scroll_view: `ScrollView::children()` is `Children::PositionalSingle`, which
    // reconciles its one child POSITIONALLY and ignores keys outright. Keyed straight onto
    // the scroll_view's child, the section switch silently diffs one section's controls into
    // another's — which re-sets each reused ComboBox's items (clearing WinUI's selection)
    // but skips `selected_index` whenever the two sections' values compare equal, so the
    // combos render blank until touched. A panel (vstack) takes the keyed path, so the key
    // remounts the whole column and every prop is applied fresh.
    let content = scroll_view(
        // ⚠️ Keyed on (scope, section), not section alone: switching SCOPE re-renders the same
        // section's controls with different values, and an in-place diff re-sets each reused
        // ComboBox's items (clearing WinUI's selection) while skipping `selected_index`
        // wherever the two scopes' values compare equal — the combo then renders blank. A
        // fresh mount applies every prop. Same reason the section key exists.
        vstack(vec![vstack(titled)
            .spacing(10.0)
            .with_key(format!("{scope}/{section}"))
            .into()])
        .margin(edges(24.0, 20.0, 28.0, 40.0)),
    )
    .opacity(progress)
    .margin(edges(0.0, (1.0 - progress) * 22.0, 0.0, 0.0));
    // The delete confirmation, when armed. Declarative, like every other dialog in this shell:
    // it is an element in the tree with `is_open`, not a call.
    let confirm: Option<Element> = delete_pending.as_ref().and_then(|id| {
        let p = ProfilesFile::load().find_by_id(id).cloned()?;
        // The warning counts what actually breaks: hosts that fall back to the defaults, and
        // pinned cards that disappear (design §6).
        let known = KnownHosts::load();
        let bound = known
            .hosts
            .iter()
            .filter(|h| h.profile_id.as_deref() == Some(p.id.as_str()))
            .count();
        let pinned = known
            .hosts
            .iter()
            .filter(|h| h.pinned_profiles.iter().any(|x| x == &p.id))
            .count();
        let mut body = format!("\u{201c}{}\u{201d} will be removed.", p.name);
        if bound > 0 {
            body.push_str(&format!(
                " {bound} host{} will fall back to Default settings.",
                if bound == 1 { "" } else { "s" }
            ));
        }
        if pinned > 0 {
            body.push_str(&format!(
                " {pinned} pinned card{} will disappear.",
                if pinned == 1 { "" } else { "s" }
            ));
        }
        let (id, set_scope, set_delete) = (p.id.clone(), set_scope.clone(), set_delete.clone());
        Some(
            ContentDialog::new("Delete profile?")
                .content(body)
                .primary_button_text("Delete")
                .close_button_text("Cancel")
                .is_open(true)
                .on_closed(move |r: ContentDialogResult| {
                    set_delete.call(None);
                    if r != ContentDialogResult::Primary {
                        return;
                    }
                    let mut catalog = ProfilesFile::load();
                    catalog.profiles.retain(|p| p.id != id);
                    // Bindings and pins are left dangling on purpose: they resolve as "no
                    // profile" everywhere, and rewriting every host record here would be a
                    // second, racier source of truth.
                    if catalog.save().is_ok() {
                        set_scope.call(String::new());
                    }
                })
                .into(),
        )
    });
    let nav = NavigationView::new(items, content)
        .pane_title("Settings")
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
        });
    match confirm {
        Some(dialog) => vstack(vec![nav.into(), dialog]).into(),
        None => nav.into(),
    }
}
