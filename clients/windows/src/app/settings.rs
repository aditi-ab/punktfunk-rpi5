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

/// The chip palette a profile can carry (`StreamProfile.accent`), same set as the GTK client so
/// a profile looks the same on both. Eight legible colours rather than a free picker: the job is
/// telling profiles apart at a glance on a host tile, and the schema still accepts any
/// `#RRGGBB` a hand-edit writes.
const SWATCHES: &[(&str, &str)] = &[
    ("", "None"),
    ("#e01b24", "Red"),
    ("#ff7800", "Orange"),
    ("#f6d32d", "Yellow"),
    ("#33d17a", "Green"),
    ("#3584e4", "Blue"),
    ("#9141ac", "Purple"),
    ("#d16d9e", "Pink"),
    ("#77767b", "Slate"),
];

/// `#RRGGBB` to a brush colour. Anything else is refused rather than guessed at — the value is
/// user data and reaches the renderer.
pub(crate) fn hex_color(hex: &str) -> Option<Color> {
    let h = hex.strip_prefix('#')?;
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(Color {
        a: 255,
        r: u8::from_str_radix(&h[0..2], 16).ok()?,
        g: u8::from_str_radix(&h[2..4], 16).ok()?,
        b: u8::from_str_radix(&h[4..6], 16).ok()?,
    })
}

/// The colour row: one tappable swatch per palette entry, the current one ringed.
fn colour_swatches(profile: &StreamProfile, rev: u64, set_rev: &SetState<u64>) -> Element {
    let current = profile.accent.clone().unwrap_or_default();
    let mut row: Vec<Element> = vec![text_block("Colour")
        .font_size(12.0)
        .foreground(ThemeRef::SecondaryText)
        .vertical_alignment(VerticalAlignment::Center)
        .margin(edges(0.0, 0.0, 6.0, 0.0))
        .into()];
    for (hex, name) in SWATCHES {
        let selected = current == *hex;
        // "None" (and anything unparsable) draws as a faint neutral disc, so the row still
        // reads as a palette with a clear "no colour" end.
        let fill = hex_color(hex).unwrap_or(Color {
            a: 40,
            r: 128,
            g: 128,
            b: 128,
        });
        let (id, set_rev, hex_owned) = (profile.id.clone(), set_rev.clone(), hex.to_string());
        row.push(
            border(vstack(Vec::<Element>::new()).width(20.0).height(20.0))
                .background(fill)
                .corner_radius(10.0)
                .border_brush(if selected {
                    ThemeRef::Accent
                } else {
                    ThemeRef::CardStroke
                })
                .border_thickness(uniform(if selected { 2.0 } else { 1.0 }))
                .tooltip(*name)
                .on_tapped(move || {
                    let mut catalog = ProfilesFile::load();
                    if let Some(p) = catalog.profiles.iter_mut().find(|p| p.id == id) {
                        p.accent = (!hex_owned.is_empty()).then(|| hex_owned.clone());
                        if let Err(e) = catalog.save() {
                            tracing::warn!(error = %format!("{e:#}"), "saving the profile colour");
                        }
                    }
                    set_rev.call(rev + 1);
                })
                .into(),
        );
    }
    hstack(row).spacing(8.0).into()
}

/// The Edit-profile modal: a scrim + centered card, the same in-tree overlay the Add-host
/// modal uses (ContentDialog is text-only in windows-reactor — no room for a text field or
/// the swatch row). Every control in it commits in place, exactly like the settings rows, so
/// the modal needs no draft state and Close is the only way out — there is nothing to cancel.
/// The one deferred repaint is the profile NAME: renaming commits as you type but the pane's
/// scope dropdown refreshes on Close (one revision bump), so the ComboBox is not remounted
/// under the user mid-keystroke.
fn edit_profile_modal(
    profile: &StreamProfile,
    set_scope: &SetState<String>,
    set_delete: &SetState<Option<String>>,
    set_edit: &SetState<bool>,
    rev: u64,
    set_rev: &SetState<u64>,
) -> Element {
    let id = profile.id.clone();
    let name_box = {
        let id = id.clone();
        text_box(&profile.name)
            .header("Name")
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
                // The modal stays open and now edits the copy — scope follows it.
                set_scope.call(new_id);
            }
        })
    };
    let delete = {
        let (id, set_delete) = (id.clone(), set_delete.clone());
        button("Delete\u{2026}").on_click(move || set_delete.call(Some(id.clone())))
    };
    let close = {
        let (set_edit, set_rev) = (set_edit.clone(), set_rev.clone());
        button("Close").accent().on_click(move || {
            set_edit.call(false);
            // The deferred repaint: the pane dropdown (and any pinned tiles) pick up the
            // rename now, in one pass, instead of remounting per keystroke.
            set_rev.call(rev + 1);
        })
    };
    let modal = dialog_surface(
        vstack((
            text_block("Edit profile").font_size(20.0).bold(),
            name_box,
            colour_swatches(profile, rev, set_rev),
            text_block(
                "A profile overrides only what you change while it is selected; everything \
                 else follows Default settings. Renaming applies as you type. Deleting leaves \
                 hosts that used it on Default settings.",
            )
            .font_size(12.0)
            .wrap()
            .foreground(ThemeRef::SecondaryText),
            hstack((duplicate, delete, close))
                .spacing(8.0)
                .horizontal_alignment(HorizontalAlignment::Right)
                .margin(edges(0.0, 6.0, 0.0, 0.0)),
        ))
        .spacing(12.0),
    )
    .max_width(420.0)
    .horizontal_alignment(HorizontalAlignment::Center)
    .vertical_alignment(VerticalAlignment::Center)
    .margin(uniform(24.0));
    // The scrim fills the cell and is hit-testable, so it blocks the page behind; it closes
    // only via the buttons (a scrim tap would bubble `Tapped` up from the card too).
    border(modal)
        .background(Color {
            a: 140,
            r: 0,
            g: 0,
            b: 0,
        })
        .into()
}

/// Persist one control's edit into the layer being edited.
///
/// This shell commits PER CONTROL (unlike the GTK one, which writes when its dialog closes),
/// so it can't hand the profile a list of touched fields. It hands over the effective settings
/// before and after instead, and [`SettingsOverlay::absorb`] records the field that moved —
/// the comparison is against what the control was SHOWING, so picking a value that happens to
/// equal the global still records an override (the pin the design asks for).
///
/// Every commit ends by bumping the revision: a profile-scope edit changes what the page
/// should SHOW (the row's Overridden marker, the catalog behind the controls) without
/// changing any state the page reads, so without the bump no render pass runs and the
/// marker only appears after some unrelated re-render — the exact bug the Linux client
/// fixed in "the override marker appears on touch". Bumping on global-scope edits too is
/// deliberate: it is one code path, a same-value repaint is cheap, and it also refreshes
/// rows whose displayed effective value derives from the field just written.
fn commit(
    ctx: &Arc<AppCtx>,
    scope: &str,
    rev: (u64, &SetState<u64>),
    edit: impl FnOnce(&mut Settings),
) {
    if scope.is_empty() {
        let mut s = ctx.settings.lock().unwrap();
        edit(&mut s);
        s.save();
        rev.1.call(rev.0 + 1);
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
    rev.1.call(rev.0 + 1);
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
    rev: (u64, &SetState<u64>),
    header: &str,
    names: Vec<String>,
    current: usize,
    apply: impl Fn(&mut Settings, usize) + 'static,
) -> ComboBox {
    let (ctx, scope) = (ctx.clone(), scope.to_string());
    let (rev, set_rev) = (rev.0, rev.1.clone());
    let max = names.len().saturating_sub(1);
    ComboBox::new(names)
        .header(header)
        .selected_index(current as i32)
        .on_selection_changed(move |i: i32| {
            commit(&ctx, &scope, (rev, &set_rev), |s| {
                apply(s, (i.max(0) as usize).min(max));
            });
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
    rev: (u64, &SetState<u64>),
    header: &str,
    on: bool,
    apply: impl Fn(&mut Settings, bool) + 'static,
) -> ToggleSwitch {
    let (ctx, scope) = (ctx.clone(), scope.to_string());
    let (rev, set_rev) = (rev.0, rev.1.clone());
    ToggleSwitch::new(on)
        .header(header)
        .on_content("On")
        .off_content("Off")
        .on_toggled(move |v: bool| {
            commit(&ctx, &scope, (rev, &set_rev), |s| apply(s, v));
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
/// says the profile changes this one, and the button is the only way back to inheriting.
/// An override is recorded when a control's committed value differs from what it was
/// SHOWING (`SettingsOverlay::absorb` diffs against the effective snapshot — see `commit`);
/// WinUI change events don't fire on a no-op re-selection, so every reachable edit marks
/// its row, and "not overridden" needs an explicit Reset. (Linux marks a literal no-op
/// touch too — unobservable here, the one intentional divergence.)
fn described_overridable(
    rev: (u64, &SetState<u64>),
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

/// Props for the settings page (mounted as its own `component(...)` so its hooks live in an
/// isolated slot list — see `Svc`'s docs in `app/mod.rs`).
///
/// What is a prop here (vs the page's own `use_state`) is deliberate:
/// * `section` + `set_section` — the selected pane tag stays ROOT state because the
///   section-switch entrance tween in `app/mod.rs` is keyed on it: the tween worker thread
///   writes root async state (`nav_anim`), and root can only start it if it owns the trigger.
///   The NavigationView's `on_selection_changed` therefore keeps calling the root setter.
/// * `progress` — that tween's value (0 → 1), root-driven for the same reason.
/// * `window_width` — read at root (`use_inner_size`) so resize re-renders the tree; the page
///   only compares it against WinUI's pane-collapse threshold.
///
/// Everything else the page shows (scope, delete confirmation, edit modal, revision) is its
/// own sync `use_state` — see `settings_page`.
#[derive(Clone)]
pub(crate) struct SettingsProps {
    pub(crate) ctx: Arc<AppCtx>,
    pub(crate) set_screen: AsyncSetState<Screen>,
    pub(crate) section: String,
    pub(crate) set_section: AsyncSetState<String>,
    pub(crate) progress: f64,
    pub(crate) window_width: f64,
}

impl PartialEq for SettingsProps {
    fn eq(&self, other: &Self) -> bool {
        // Setters are identity-stable; only the data fields drive re-render.
        Arc::ptr_eq(&self.ctx, &other.ctx)
            && self.section == other.section
            && self.progress == other.progress
            && self.window_width == other.window_width
    }
}

/// The settings screen: a stock WinUI `NavigationView` (the Windows-Settings sidebar pattern) —
/// one pane item per section, the section's card as the content, the built-in back arrow
/// returning to the host list.
///
/// The page-only UI state (which layer is edited, the delete confirmation, the Edit-profile
/// modal, the repaint revision) lives HERE as sync `use_state`, de-hoisted from root
/// 2026-07-29: every writer is a UI-thread handler (ComboBox `on_selection_changed`, button
/// clicks, swatch taps), and the characterization suite measured that a sync `use_state`
/// write from a backend-wired handler re-renders its owning component
/// (`tests/reactor_semantics.rs::sync_state_from_backend_fired_event_rerenders`). The section
/// tag is the exception — root-owned, tween-coupled; see `SettingsProps`.
pub(crate) fn settings_page(props: &SettingsProps, cx: &mut RenderCx) -> Element {
    // Hooks first, unconditionally, in a stable order (reactor's Rules-of-Hooks guard aborts
    // on a changed order).
    // Which LAYER the screen edits: "" = the global defaults, else a profile id
    // (design/client-settings-profiles.md §5.1).
    let (scope_id, set_scope) = cx.use_state(String::new());
    // The profile a Delete… click is asking about; `Some` renders the confirmation.
    let (delete_pending, set_delete) = cx.use_state(Option::<String>::None);
    // Whether the Edit-profile modal is up; guarded below so it only renders while a profile
    // is actually in scope.
    let (edit_open, set_edit) = cx.use_state(false);
    // Bumped when an edit changes what the page should SHOW without changing any state it
    // already reads — ANY edit through `commit` (creating an override must surface its marker
    // as immediately as resetting one clears it), a reset, a profile colour change. The
    // same-value compare in `call` makes no-op bumps free; the counter is what forces the pass.
    let (rev, set_rev) = cx.use_state(0u64);
    let (set_scope, set_delete, set_edit, set_rev) = (&set_scope, &set_delete, &set_edit, &set_rev);
    let ctx = &props.ctx;
    let set_screen = &props.set_screen;
    let section = props.section.as_str();
    let set_section = &props.set_section;
    let progress = props.progress;
    let window_width = props.window_width;
    // The layer being edited. A scope pointing at a deleted profile degrades to the defaults,
    // the same rule a dangling host binding follows.
    let active = active_profile(&scope_id);
    let scope: &str = match &active {
        Some(p) => &p.id,
        None => "",
    };
    let profile_mode = active.is_some();
    // Which rows this profile overrides — the marker + reset each of them carries. In the
    // defaults scope nothing is marked, and `described_overridable` degrades to `described`.
    let over = OverrideFlags::of(active.as_ref());
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
    let res_combo = setting_combo(
        ctx,
        scope,
        (rev, set_rev),
        "Resolution",
        res_names,
        res_i,
        |s, i| {
            s.match_window = i == 1;
            (s.width, s.height) = if i <= 1 { (0, 0) } else { RESOLUTIONS[i - 1] };
        },
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
    let hz_combo = setting_combo(
        ctx,
        scope,
        (rev, set_rev),
        "Refresh rate",
        hz_names,
        hz_i,
        |s, i| {
            s.refresh_hz = REFRESH[i];
        },
    );
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
    let scale_combo = setting_combo(
        ctx,
        scope,
        (rev, set_rev),
        "Render scale",
        scale_names,
        scale_i,
        |s, i| {
            s.render_scale = RENDER_SCALES[i];
        },
    );
    let (comp_names, comp_i) = presets(COMPOSITORS, |v| *v == s.compositor);
    let comp_combo = setting_combo(
        ctx,
        scope,
        (rev, set_rev),
        "Host compositor",
        comp_names,
        comp_i,
        |s, i| {
            s.compositor = COMPOSITORS[i].0.to_string();
        },
    );
    let auto_wake_toggle = setting_toggle(
        ctx,
        scope,
        (rev, set_rev),
        "Auto-wake on connect",
        s.auto_wake,
        |s, on| s.auto_wake = on,
    );
    let fullscreen_toggle = setting_toggle(
        ctx,
        scope,
        (rev, set_rev),
        "Start streams fullscreen",
        s.fullscreen_on_stream,
        |s, on| s.fullscreen_on_stream = on,
    );

    // --- Video -----------------------------------------------------------------------------
    let (dec_names, dec_i) = presets(DECODERS, |v| *v == s.decoder);
    let decoder_combo = setting_combo(
        ctx,
        scope,
        (rev, set_rev),
        "Video decoder",
        dec_names,
        dec_i,
        |s, i| {
            s.decoder = DECODERS[i].0.to_string();
        },
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
        setting_combo(
            ctx,
            scope,
            (rev, set_rev),
            "GPU",
            names,
            current,
            move |s, i| {
                s.adapter = if i == 0 {
                    String::new()
                } else {
                    gpus[i - 1].clone()
                };
            },
        )
    });
    let (codec_names, codec_i) = presets(CODECS, |v| *v == s.codec);
    let codec_combo = setting_combo(
        ctx,
        scope,
        (rev, set_rev),
        "Video codec",
        codec_names,
        codec_i,
        |s, i| {
            s.codec = CODECS[i].0.to_string();
        },
    );
    // Free-form Mb/s (0 = host default) instead of presets, so a speed-test recommendation
    // round-trips exactly. Through `commit` like every other row: writing `ctx.settings`
    // directly here would edit the GLOBAL defaults from inside a profile scope (and record
    // no override, so the row could never say "Overridden here").
    let bitrate_box = {
        let (ctx, scope, set_rev) = (ctx.clone(), scope.to_string(), set_rev.clone());
        NumberBox::new(f64::from(s.bitrate_kbps) / 1000.0)
            .header("Bitrate (Mb/s, 0 = automatic)")
            .range(0.0, 3000.0)
            .on_value_changed(move |v: f64| {
                commit(&ctx, &scope, (rev, &set_rev), |s| {
                    s.bitrate_kbps = (v.clamp(0.0, 3000.0) * 1000.0) as u32;
                });
            })
    };
    let hdr_toggle = setting_toggle(
        ctx,
        scope,
        (rev, set_rev),
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
    let pad_combo = setting_combo(
        ctx,
        scope,
        (rev, set_rev),
        "Gamepad type",
        pad_names,
        pad_i,
        |s, i| {
            s.gamepad = GAMEPADS[i].0.to_string();
        },
    );
    let (touch_names, touch_i) = presets(TOUCH_MODES, |v| *v == s.touch_mode);
    let touch_combo = setting_combo(
        ctx,
        scope,
        (rev, set_rev),
        "Touch input",
        touch_names,
        touch_i,
        |s, i| {
            s.touch_mode = TOUCH_MODES[i].0.to_string();
        },
    );
    let (mouse_names, mouse_i) = presets(MOUSE_MODES, |v| *v == s.mouse_mode);
    let mouse_combo = setting_combo(
        ctx,
        scope,
        (rev, set_rev),
        "Mouse input",
        mouse_names,
        mouse_i,
        |s, i| {
            s.mouse_mode = MOUSE_MODES[i].0.to_string();
        },
    );
    let invert_scroll_toggle = setting_toggle(
        ctx,
        scope,
        (rev, set_rev),
        "Invert scroll direction",
        s.invert_scroll,
        |s, on| s.invert_scroll = on,
    );
    let shortcuts_toggle = setting_toggle(
        ctx,
        scope,
        (rev, set_rev),
        "Capture system shortcuts (Alt+Tab, Win, \u{2026})",
        s.inhibit_shortcuts,
        |s, on| s.inhibit_shortcuts = on,
    );

    // --- Audio -----------------------------------------------------------------------------
    let (ac_names, ac_i) = presets(AUDIO_CHANNELS, |v| *v == s.audio_channels);
    let channels_combo = setting_combo(
        ctx,
        scope,
        (rev, set_rev),
        "Audio channels",
        ac_names,
        ac_i,
        |s, i| {
            s.audio_channels = AUDIO_CHANNELS[i].0;
        },
    );
    let mic_toggle = setting_toggle(
        ctx,
        scope,
        (rev, set_rev),
        "Stream microphone to the host",
        s.mic_enabled,
        |s, on| s.mic_enabled = on,
    );

    let (hud_names, hud_i) = presets(STATS_TIERS, |v| *v == s.stats_verbosity());
    let hud_combo = setting_combo(
        ctx,
        scope,
        (rev, set_rev),
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
        (rev, set_rev),
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
    // The scope switcher lives in the NavigationView's PANE FOOTER: it is chrome ("which
    // layer am I editing"), visible from every section, and down there it reads as part of
    // the navigation rather than as one more settings row — a bar of profile controls above
    // the whole surface read as clutter. Editing the profile itself (name, colour, delete)
    // is a modal behind the "Edit profile…" button, not inline chrome.
    //
    // But ONLY while the pane is expanded — pane-footer content is CLIPPED to the compact
    // rail, not adapted. WinUI's Auto mode would collapse the pane below 1008 epx (under the
    // app's own 1000-wide default window!), which kept booting the switcher out of the nav,
    // so the page FORCES the pane: expanded (`Left`, at a tighter 250 length) whenever the
    // content column still gets a workable width, minimal below that. Reactor exposes no
    // pane-opened/closed event, so the switcher placement rides the same threshold.
    let pane_expanded = window_width >= 720.0;
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
    let switcher = ComboBox::new(scope_names.clone())
        .header("Editing")
        .selected_index(scope_i as i32)
        .min_width(200.0)
        .on_selection_changed({
            let (set_scope, set_edit, ids) = (set_scope.clone(), set_edit.clone(), scope_ids);
            move |i: i32| {
                let Some(id) = ids.get(i.max(0) as usize) else {
                    return;
                };
                if id == NEW_PROFILE {
                    // A new profile takes an auto-numbered name and lands straight in the
                    // Edit modal to be named — creation and naming are one gesture, and
                    // there is no half-created state a Cancel would have to unwind.
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
                        set_edit.call(true);
                    }
                    return;
                }
                set_scope.call(id.clone());
            }
        });
    let edit_button: Option<Element> = profile_mode.then(|| {
        let set_edit = set_edit.clone();
        button("Edit profile\u{2026}")
            .on_click(move || set_edit.call(true))
            .into()
    });
    // Keyed by scope + the name list: a rename/create/delete changes the ComboBox's items,
    // and an in-place diff re-sets items (clearing WinUI's selection) while skipping
    // `selected_index` when it compares equal — the combo then renders blank. A remount
    // applies every prop. The keyed child sits inside a plain vstack because only a panel's
    // child list takes the keyed path (same rule as the content column below).
    let switcher_key = format!("{scope}\u{1}{}", scope_names.join("\u{1}"));
    // Expanded pane: combo over button, in the footer. Collapsed: one horizontal row that
    // leads the content column (bottom-aligned so the combo's "Editing" header doesn't push
    // the button off its baseline).
    let (footer, inline_switcher): (Option<Element>, Option<Element>) = if pane_expanded {
        let rows: Vec<Element> = std::iter::once(Element::from(switcher))
            .chain(edit_button)
            .collect();
        (
            Some(
                vstack(vec![vstack(rows)
                    .spacing(8.0)
                    .with_key(switcher_key)
                    .into()])
                .margin(edges(16.0, 0.0, 16.0, 12.0))
                .into(),
            ),
            None,
        )
    } else {
        let row: Vec<Element> = std::iter::once(Element::from(switcher))
            .chain(edit_button)
            .collect();
        (
            None,
            Some(
                vstack(vec![hstack(row)
                    .spacing(8.0)
                    .vertical_alignment(VerticalAlignment::Bottom)
                    .with_key(switcher_key)
                    .into()])
                .margin(edges(24.0, 12.0, 28.0, 0.0))
                .into(),
            ),
        )
    };

    let titled: Vec<Element> = std::iter::once(
        text_block(title)
            .font_size(28.0)
            .semibold()
            .horizontal_alignment(HorizontalAlignment::Left)
            .margin(edges(0.0, 0.0, 0.0, 6.0))
            .into(),
    )
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
    let scrolled = scroll_view(
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
    // With the pane collapsed the switcher leads the content column, above the scrolling
    // region so it stays put — it is still chrome, not a settings row. An Auto/Star grid,
    // not a vstack: a StackPanel would hand the scroll_view its DESIRED height and the
    // viewport (and scrolling) would be gone.
    let content: Element = match inline_switcher {
        Some(row) => grid(vec![row.grid_row(0), Element::from(scrolled).grid_row(1)])
            .rows([GridLength::Auto, GridLength::STAR])
            .into(),
        None => scrolled.into(),
    };
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
        let (id, set_scope, set_delete, set_edit) = (
            p.id.clone(),
            set_scope.clone(),
            set_delete.clone(),
            set_edit.clone(),
        );
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
                        // The profile the Edit modal was showing is gone — without this, the
                        // still-armed flag would pop the modal open on the NEXT profile pick.
                        set_edit.call(false);
                    }
                })
                .into(),
        )
    });
    let mut nav = NavigationView::new(items, content)
        .pane_title("Settings")
        .pane_display_mode(if pane_expanded {
            NavigationViewPaneDisplayMode::Left
        } else {
            NavigationViewPaneDisplayMode::LeftMinimal
        })
        .open_pane_length(250.0)
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
    if let Some(f) = footer {
        nav = nav.pane_footer(f);
    }
    // One grid, so every layer FILLS the window (a vstack would hand the NavigationView its
    // desired height — clipped when the window is short, floating when it is tall): the nav,
    // the Edit-profile scrim + card over it, and the delete confirmation (a ContentDialog,
    // its own WinUI layer, so its slot in the cell is irrelevant — it just needs to be in
    // the tree).
    let mut layers: Vec<Element> = vec![nav.into()];
    if edit_open {
        if let Some(p) = &active {
            layers.push(edit_profile_modal(
                p, set_scope, set_delete, set_edit, rev, set_rev,
            ));
        }
    }
    if let Some(dialog) = confirm {
        layers.push(dialog);
    }
    grid(layers).into()
}
