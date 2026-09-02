//! Preferences dialog on the cross-client category map (the Apple 2026-07 settings
//! revamp): General / Display / Input / Audio / Controllers pages — Display owns
//! everything about the picture — with per-field captions in each row's subtitle,
//! dynamic where the meaning depends on the selection (touch mode). Written back to
//! disk when the dialog closes. About stays in the primary menu (GNOME convention)
//! rather than as a page.
//!
//! The same surface edits SETTINGS PROFILES (design/client-settings-profiles.md §5.1): a
//! scope switcher at the top swaps the whole dialog between the global defaults and one
//! profile's overrides. It is deliberately not a second editor — a parallel one would drift
//! from this one field by field. In profile scope only profileable ("tier P") rows render,
//! every row shows the EFFECTIVE value (the inherited global until you touch it), and the
//! override is recorded on touch rather than by comparing values, so a profile can pin a
//! value that happens to equal today's global and keep it when the global later moves.

use crate::trust::Settings;
use adw::prelude::*;
use pf_client_core::profiles::{ProfilesFile, SettingsOverlay, StreamProfile};
// The audio-format table lives in the session crate, not here, because the same three stored
// values also have to reach the wire — and they are shared verbatim with the Apple and Android
// clients so one profile round-trips. A second copy of the spellings in this file is exactly the
// drift the shared table exists to prevent.
use pf_client_core::session::AUDIO_FORMATS;
use pf_client_core::trust::StatsVerbosity;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

/// Which layer the dialog is editing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The global defaults every profile inherits from — the only scope before this feature.
    Defaults,
    /// One profile's overrides, by id.
    Profile(String),
}

/// Which rows the user actually touched this session. The override model is explicit, not
/// diffed: touching a control creates the override, and only an explicit reset removes it
/// (design §4.1). Keys are the overlay's field names, with `resolution` covering the
/// width/height/match-window tri-state that one row drives.
#[derive(Clone, Default)]
struct Touched {
    keys: Rc<RefCell<HashSet<&'static str>>>,
    /// Set while a control is being changed BY US (putting a reset row back to the inherited
    /// value). Without it the programmatic change would read as the user touching the row and
    /// immediately re-create the override the reset just removed.
    suspended: Rc<std::cell::Cell<bool>>,
}

impl Touched {
    fn mark(&self, key: &'static str) {
        self.keys.borrow_mut().insert(key);
    }

    fn suspended(&self) -> bool {
        self.suspended.get()
    }

    fn set_suspended(&self, v: bool) {
        self.suspended.set(v);
    }

    fn has(&self, key: &str) -> bool {
        self.keys.borrow().contains(key)
    }

    /// Undo a touch — the user reset this row after changing it, and "back to inheriting" is
    /// the later, explicit intent.
    fn forget(&self, key: &str) {
        self.keys.borrow_mut().remove(key);
    }
}

/// `(0, 0)` = the native size of the monitor the window is on, resolved at connect.
const RESOLUTIONS: &[(u32, u32)] = &[
    (0, 0),
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];
/// `0` = the monitor's native refresh, resolved at connect.
const REFRESH: &[u32] = &[0, 30, 60, 90, 120, 144, 165, 240];
/// Render-scale multipliers (persisted as f64; mirrors [`punktfunk_core::render_scale::PRESETS`]).
/// `1.0` = Native. Applied at connect and each match-window resize.
const RENDER_SCALES: &[f64] = &[0.5, 0.67, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0];

/// Where each picker sits for a given settings snapshot. Factored out because two places need
/// exactly this: seeding the dialog, and putting ONE row back to the inherited value when its
/// override is reset — and those two must agree, or a reset would land on a different option
/// than reopening the dialog would show.
mod index {
    use super::*;

    pub fn resolution(s: &Settings) -> u32 {
        // Index 1 is the virtual "Match window" entry; 0 = Native, 2.. = the explicit sizes.
        let i = if s.match_window {
            1
        } else {
            RESOLUTIONS
                .iter()
                .position(|&(w, h)| w == s.width && h == s.height)
                .map(|i| if i == 0 { 0 } else { i + 1 })
                .unwrap_or(0)
        };
        i as u32
    }

    pub fn refresh(s: &Settings) -> u32 {
        REFRESH.iter().position(|&r| r == s.refresh_hz).unwrap_or(0) as u32
    }

    pub fn render_scale(s: &Settings) -> u32 {
        RENDER_SCALES
            .iter()
            .position(|&x| (x - s.render_scale).abs() < 1e-6)
            .unwrap_or_else(|| RENDER_SCALES.iter().position(|&x| x == 1.0).unwrap()) as u32
    }

    pub fn codec(s: &Settings) -> u32 {
        CODECS.iter().position(|&c| c == s.codec).unwrap_or(0) as u32
    }

    pub fn compositor(s: &Settings) -> u32 {
        COMPOSITORS
            .iter()
            .position(|&c| c == s.compositor)
            .unwrap_or(0) as u32
    }

    pub fn stats(s: &Settings) -> u32 {
        StatsVerbosity::ALL
            .iter()
            .position(|v| *v == s.stats_verbosity())
            .unwrap_or(0) as u32
    }

    pub fn touch(s: &Settings) -> u32 {
        TOUCH_MODES
            .iter()
            .position(|&t| t == s.touch_mode)
            .unwrap_or(0) as u32
    }

    pub fn mouse(s: &Settings) -> u32 {
        MOUSE_MODES
            .iter()
            .position(|&m| m == s.mouse_mode)
            .unwrap_or(0) as u32
    }

    pub fn surround(s: &Settings) -> u32 {
        match s.audio_channels {
            6 => 1,
            8 => 2,
            _ => 0,
        }
    }

    /// An unknown stored value (a newer client's row, via a shared profile) reads as row 0 —
    /// Opus — which is also what the session resolves it to, so the row and the wire agree.
    pub fn audio_format(s: &Settings) -> u32 {
        AUDIO_FORMATS
            .iter()
            .position(|(v, _)| *v == s.audio_format)
            .unwrap_or(0) as u32
    }

    pub fn gamepad(s: &Settings) -> u32 {
        GAMEPADS.iter().position(|&g| g == s.gamepad).unwrap_or(0) as u32
    }

    pub fn system_buttons(s: &Settings) -> u32 {
        SYSTEM_BUTTONS
            .iter()
            .position(|&v| v == s.system_buttons)
            .unwrap_or(0) as u32
    }

    pub fn guide_gesture(s: &Settings) -> u32 {
        GUIDE_GESTURES
            .iter()
            .position(|&v| v == s.guide_gesture)
            .unwrap_or(0) as u32
    }

    pub fn present_priority(s: &Settings) -> u32 {
        // Unknown values (a newer client's intent) read as the default, exactly as
        // `PresentPriority::resolve` treats them.
        PRESENT_PRIORITIES
            .iter()
            .position(|&p| p == s.present_priority)
            .unwrap_or(0) as u32
    }

    pub fn smooth_buffer(s: &Settings) -> u32 {
        // The index IS the stored value: 0 = Automatic, 1..3 = frames.
        u32::from(s.smooth_buffer).min(SMOOTH_BUFFER_LABELS.len() as u32 - 1)
    }
}

/// The chip palette a profile can carry (`StreamProfile.accent`). Eight entries rather than a
/// full colour picker: the point is telling profiles apart at a glance on a host card, which a
/// small set of legible, contrast-checked colours does better than free choice — and the
/// schema's `#RRGGBB` still accepts anything a future picker or a hand-edit writes.
const SWATCHES: &[(&str, &str, &str)] = &[
    ("", "pf-swatch-none", "No colour"),
    ("#e01b24", "pf-swatch-red", "Red"),
    ("#ff7800", "pf-swatch-orange", "Orange"),
    ("#f6d32d", "pf-swatch-yellow", "Yellow"),
    ("#33d17a", "pf-swatch-green", "Green"),
    ("#3584e4", "pf-swatch-blue", "Blue"),
    ("#9141ac", "pf-swatch-purple", "Purple"),
    ("#d16d9e", "pf-swatch-pink", "Pink"),
    ("#77767b", "pf-swatch-slate", "Slate"),
];

/// A colour picker as its own full-width row: the caption above, the swatches on their own
/// line beneath, wrapping if the dialog is narrow.
///
/// They started as an [`adw::ActionRow`] suffix, which is where a control that size normally
/// goes — but nine swatches squeeze the row's title until it is unreadable, and squeezing the
/// label to fit the control is backwards. A `FlowBox` beneath keeps both legible at any width.
fn colour_row(
    title: &str,
    current: Option<&str>,
    on_pick: impl Fn(String) + 'static,
) -> adw::PreferencesRow {
    let box_ = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    box_.append(
        &gtk::Label::builder()
            .label(title)
            .halign(gtk::Align::Start)
            .build(),
    );
    box_.append(&swatch_row(current, on_pick));
    adw::PreferencesRow::builder()
        .activatable(false)
        .child(&box_)
        .build()
}

/// The swatches themselves, calling back with the chosen `#RRGGBB` (empty = none).
fn swatch_row(current: Option<&str>, on_pick: impl Fn(String) + 'static) -> gtk::FlowBox {
    let row = gtk::FlowBox::builder()
        .orientation(gtk::Orientation::Horizontal)
        .selection_mode(gtk::SelectionMode::None)
        .column_spacing(8)
        .row_spacing(8)
        .min_children_per_line(5)
        .max_children_per_line(9)
        // NOT homogeneous, and start-aligned: a homogeneous FlowBox stretches every child to
        // the widest one, which turns 26px circles into wide rounded rectangles the moment the
        // row has space to spare.
        .homogeneous(false)
        .halign(gtk::Align::Start)
        .build();
    let on_pick = Rc::new(on_pick);
    let buttons: Rc<RefCell<Vec<(String, gtk::Button)>>> = Rc::default();
    for (hex, class, name) in SWATCHES {
        let b = gtk::Button::builder()
            .css_classes(["pf-swatch", class, "circular"])
            .tooltip_text(*name)
            .valign(gtk::Align::Center)
            .build();
        if current.unwrap_or("") == *hex {
            b.add_css_class("pf-swatch-on");
        }
        {
            let (on_pick, buttons, hex) = (on_pick.clone(), buttons.clone(), hex.to_string());
            b.connect_clicked(move |me| {
                // The selection ring is exclusive, so clear every sibling before setting ours.
                for (_, other) in buttons.borrow().iter() {
                    other.remove_css_class("pf-swatch-on");
                }
                me.add_css_class("pf-swatch-on");
                on_pick(hex.clone());
            });
        }
        buttons.borrow_mut().push((hex.to_string(), b.clone()));
        row.insert(&b, -1);
    }
    row
}

/// The scope switcher, plus (in profile scope) that profile's management actions.
///
/// Switching scope does not swap the rows in place: it closes the dialog — which commits the
/// layer being edited — and asks the app to re-open in the new scope. One code path builds
/// the rows, and the commit ordering is unambiguous.
#[allow(clippy::too_many_arguments)]
fn scope_group(
    dialog: &adw::PreferencesDialog,
    inline: bool,
    scope: &Scope,
    catalog: &ProfilesFile,
    active: Option<&StreamProfile>,
    next_scope: &Rc<RefCell<Option<Scope>>>,
    parent: &impl IsA<gtk::Widget>,
) -> adw::PreferencesGroup {
    let g = group(
        "",
        "A profile overrides only what you change here; everything else follows Default \
         settings.",
    );
    let mut labels: Vec<String> = vec!["Default settings".into()];
    labels.extend(catalog.profiles.iter().map(|p| p.name.clone()));
    labels.push("New profile…".into());
    let new_index = (labels.len() - 1) as u32;
    let current = match scope {
        Scope::Defaults => 0,
        Scope::Profile(id) => catalog
            .profiles
            .iter()
            .position(|p| &p.id == id)
            .map_or(0, |i| i as u32 + 1),
    };
    let row = ChoiceRow::new(
        dialog,
        inline,
        "Editing",
        "Which layer these settings belong to",
        &labels.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    row.set_selected(current);
    {
        let (dialog, next, parent) = (dialog.clone(), next_scope.clone(), parent.as_ref().clone());
        let ids: Vec<String> = catalog.profiles.iter().map(|p| p.id.clone()).collect();
        row.connect_changed(move |i| {
            if i == new_index {
                // Creation is the one branch that has to ask a question first; the switch
                // happens in its callback, so a cancelled prompt leaves the dialog put.
                let (dialog, next) = (dialog.clone(), next.clone());
                prompt_name(
                    &parent,
                    "New profile",
                    "Create",
                    "",
                    true,
                    move |name, accent| {
                        let mut catalog = ProfilesFile::load();
                        if catalog.name_taken(&name, None) {
                            return; // the prompt already refuses these; belt and braces
                        }
                        let mut profile = StreamProfile::new(name);
                        profile.accent = accent;
                        let id = profile.id.clone();
                        catalog.profiles.push(profile);
                        if catalog.save().is_ok() {
                            *next.borrow_mut() = Some(Scope::Profile(id));
                            dialog.close();
                        }
                    },
                );
                return;
            }
            *next.borrow_mut() = Some(match i {
                0 => Scope::Defaults,
                n => match ids.get(n as usize - 1) {
                    Some(id) => Scope::Profile(id.clone()),
                    None => Scope::Defaults,
                },
            });
            dialog.close();
        });
    }
    g.add(row.widget());
    // Leaking the row keeps its handler alive for the dialog's lifetime — the ChoiceRow owns
    // the closure, and the widget alone doesn't keep it.
    std::mem::forget(row);

    if let Some(active) = active {
        // Colour first: it is what the profile's chips carry on host cards, so it belongs with
        // the profile's identity rather than buried behind a menu.
        g.add(&colour_row(
            "Colour \u{2014} tints this profile's chips on host cards",
            active.accent.as_deref(),
            {
                let id = active.id.clone();
                move |hex| {
                    let mut catalog = ProfilesFile::load();
                    if let Some(p) = catalog.profiles.iter_mut().find(|p| p.id == id) {
                        p.accent = (!hex.is_empty()).then(|| hex.clone());
                        if let Err(e) = catalog.save() {
                            tracing::warn!(error = %format!("{e:#}"), "saving the profile colour");
                        }
                    }
                }
            },
        ));
        let actions = adw::ActionRow::builder()
            .title(&active.name)
            .subtitle("This profile")
            .use_markup(false)
            .build();
        let buttons = gtk::Box::builder()
            .spacing(6)
            .valign(gtk::Align::Center)
            .build();
        for (label, action) in [
            ("Rename…", ProfileAction::Rename),
            ("Duplicate", ProfileAction::Duplicate),
            ("Delete…", ProfileAction::Delete),
        ] {
            let b = gtk::Button::builder().label(label).build();
            if matches!(action, ProfileAction::Delete) {
                b.add_css_class("destructive-action");
            }
            let (dialog, next, parent, id, name) = (
                dialog.clone(),
                next_scope.clone(),
                parent.as_ref().clone(),
                active.id.clone(),
                active.name.clone(),
            );
            b.connect_clicked(move |_| {
                run_profile_action(action, &parent, &dialog, &next, &id, &name)
            });
            buttons.append(&b);
        }
        actions.add_suffix(&buttons);
        g.add(&actions);
    }
    g
}

#[derive(Clone, Copy)]
enum ProfileAction {
    Rename,
    Duplicate,
    Delete,
}

/// Rename / duplicate / delete for the profile in scope. Each ends by closing the dialog so
/// the edit and the re-render can't disagree about what the catalog holds.
fn run_profile_action(
    action: ProfileAction,
    parent: &gtk::Widget,
    dialog: &adw::PreferencesDialog,
    next: &Rc<RefCell<Option<Scope>>>,
    id: &str,
    name: &str,
) {
    let (dialog, next, id) = (dialog.clone(), next.clone(), id.to_string());
    match action {
        ProfileAction::Rename => {
            let keep = id.clone();
            prompt_name(
                parent,
                "Rename profile",
                "Rename",
                name,
                false,
                move |new_name, _| {
                    let mut catalog = ProfilesFile::load();
                    if catalog.name_taken(&new_name, Some(&keep)) {
                        return;
                    }
                    if let Some(p) = catalog.profiles.iter_mut().find(|p| p.id == keep) {
                        p.name = new_name;
                    }
                    if catalog.save().is_ok() {
                        *next.borrow_mut() = Some(Scope::Profile(keep.clone()));
                        dialog.close();
                    }
                },
            );
        }
        ProfileAction::Duplicate => {
            let mut catalog = ProfilesFile::load();
            let Some(source) = catalog.find_by_id(&id).cloned() else {
                return;
            };
            // "Work 2", "Work 3", … — the first name the catalog doesn't already hold.
            let copy_name = (2..)
                .map(|n| format!("{} {n}", source.name))
                .find(|n| !catalog.name_taken(n, None))
                .unwrap_or_else(|| source.name.clone());
            let mut copy = StreamProfile::new(copy_name);
            copy.overrides = source.overrides.clone();
            copy.accent = source.accent.clone();
            let new_id = copy.id.clone();
            catalog.profiles.push(copy);
            if catalog.save().is_ok() {
                *next.borrow_mut() = Some(Scope::Profile(new_id));
                dialog.close();
            }
        }
        ProfileAction::Delete => {
            // The warning counts what actually breaks: hosts that fall back to the defaults,
            // and pinned cards that disappear (design §6).
            let known = crate::trust::KnownHosts::load();
            let bound = known
                .hosts
                .iter()
                .filter(|h| h.profile_id.as_deref() == Some(id.as_str()))
                .count();
            let pinned = known
                .hosts
                .iter()
                .filter(|h| h.pinned_profiles.iter().any(|p| p == &id))
                .count();
            let mut body = format!("“{name}” will be removed.");
            if bound > 0 {
                body.push_str(&format!(
                    "\n\n{bound} host{} will fall back to Default settings.",
                    if bound == 1 { "" } else { "s" }
                ));
            }
            if pinned > 0 {
                body.push_str(&format!(
                    "\n{pinned} pinned card{} will disappear.",
                    if pinned == 1 { "" } else { "s" }
                ));
            }
            let confirm = adw::AlertDialog::new(Some("Delete profile?"), Some(&body));
            confirm.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
            confirm.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
            confirm.set_close_response("cancel");
            confirm.connect_response(Some("delete"), move |_, _| {
                let mut catalog = ProfilesFile::load();
                catalog.profiles.retain(|p| p.id != id);
                if catalog.save().is_ok() {
                    // Bindings and pins are left dangling on purpose: they resolve as "no
                    // profile" everywhere, and rewriting every host record here would be a
                    // second, racier source of truth.
                    *next.borrow_mut() = Some(Scope::Defaults);
                    dialog.close();
                }
            });
            confirm.present(Some(parent));
        }
    }
}

/// A one-line name prompt (create/rename). Refuses empty and duplicate names in place —
/// menus keyed by name are ambiguous otherwise (design §6) — rather than failing after the
/// dialog is gone.
fn prompt_name(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    accept: &str,
    initial: &str,
    with_colour: bool,
    on_ok: impl Fn(String, Option<String>) + 'static,
) {
    let dialog = adw::AlertDialog::new(Some(heading), None);
    let entry = adw::EntryRow::builder().title("Name").build();
    entry.set_text(initial);
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.append(&entry);
    // Creating a profile picks its colour here, in the same breath as its name — going hunting
    // for it afterwards is exactly the friction that leaves every profile grey.
    let accent: Rc<RefCell<Option<String>>> = Rc::default();
    if with_colour {
        list.append(&colour_row("Colour", None, {
            let accent = accent.clone();
            move |hex| *accent.borrow_mut() = (!hex.is_empty()).then(|| hex.clone())
        }));
    }
    dialog.set_extra_child(Some(&list));
    dialog.add_responses(&[("cancel", "Cancel"), ("ok", accept)]);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    let taken_against = initial.to_string();
    let e = entry.clone();
    let d = dialog.clone();
    let validate = move || {
        let name = e.text().trim().to_string();
        let catalog = ProfilesFile::load();
        let dup = !name.eq_ignore_ascii_case(&taken_against) && catalog.name_taken(&name, None);
        d.set_response_enabled("ok", !name.is_empty() && !dup);
        e.set_title(if dup {
            "Name — already used by another profile"
        } else {
            "Name"
        });
    };
    validate();
    {
        let validate = validate.clone();
        entry.connect_changed(move |_| validate());
    }
    let e = entry.clone();
    dialog.connect_response(Some("ok"), move |_, _| {
        let name = e.text().trim().to_string();
        if !name.is_empty() {
            on_ok(name, accent.borrow().clone());
        }
    });
    dialog.present(Some(parent));
}

/// Write the rows the user touched into this profile's overlay and persist the catalog.
///
/// Only touched fields move: an untouched row leaves whatever the profile already had
/// (an inherited `None`, or an existing override this build might not even render), which is
/// what keeps an older client from erasing a newer one's values just by opening the dialog.
/// The catalog is re-read here rather than reused, so a profile renamed in another window
/// between opening and closing this one survives.
fn commit_profile(active: &StreamProfile, touched: &Touched, values: &Settings) {
    let mut catalog = ProfilesFile::load();
    let Some(slot) = catalog.profiles.iter_mut().find(|p| p.id == active.id) else {
        return; // deleted from under us — nothing to write to, and nothing to complain about
    };
    let o: &mut SettingsOverlay = &mut slot.overrides;
    if touched.has("resolution") {
        // One row drives the tri-state, so all three fields move together.
        o.match_window = Some(values.match_window);
        o.width = Some(values.width);
        o.height = Some(values.height);
    }
    if touched.has("refresh_hz") {
        o.refresh_hz = Some(values.refresh_hz);
    }
    if touched.has("render_scale") {
        o.render_scale = Some(values.render_scale);
    }
    if touched.has("bitrate_kbps") {
        o.bitrate_kbps = Some(values.bitrate_kbps);
    }
    if touched.has("codec") {
        o.codec = Some(values.codec.clone());
    }
    if touched.has("hdr_enabled") {
        o.hdr_enabled = Some(values.hdr_enabled);
    }
    if touched.has("enable_444") {
        o.enable_444 = Some(values.enable_444);
    }
    if touched.has("ten_bit_sdr") {
        o.ten_bit_sdr = Some(values.ten_bit_sdr);
    }
    if touched.has("compositor") {
        o.compositor = Some(values.compositor.clone());
    }
    if touched.has("audio_channels") {
        o.audio_channels = Some(values.audio_channels);
    }
    if touched.has("audio_format") {
        o.audio_format = Some(values.audio_format.clone());
    }
    if touched.has("keep_host_audio") {
        o.keep_host_audio = Some(values.keep_host_audio);
    }
    if touched.has("mic_enabled") {
        o.mic_enabled = Some(values.mic_enabled);
    }
    if touched.has("echo_cancel") {
        o.echo_cancel = Some(values.echo_cancel);
    }
    if touched.has("touch_mode") {
        o.touch_mode = Some(values.touch_mode.clone());
    }
    if touched.has("mouse_mode") {
        o.mouse_mode = Some(values.mouse_mode.clone());
    }
    if touched.has("invert_scroll") {
        o.invert_scroll = Some(values.invert_scroll);
    }
    if touched.has("inhibit_shortcuts") {
        o.inhibit_shortcuts = Some(values.inhibit_shortcuts);
    }
    if touched.has("gamepad") {
        o.gamepad = Some(values.gamepad.clone());
    }
    if touched.has("gamepad_forwarding") {
        o.gamepad_forwarding = Some(values.gamepad_forwarding);
    }
    if touched.has("system_buttons") {
        o.system_buttons = Some(values.system_buttons.clone());
    }
    if touched.has("guide_gesture") {
        o.guide_gesture = Some(values.guide_gesture.clone());
    }
    if touched.has("stats_verbosity") {
        o.stats_verbosity = Some(values.stats_verbosity());
    }
    if touched.has("fullscreen_on_stream") {
        o.fullscreen_on_stream = Some(values.fullscreen_on_stream);
    }
    if touched.has("present_priority") {
        o.present_priority = Some(values.present_priority.clone());
    }
    if touched.has("smooth_buffer") {
        o.smooth_buffer = Some(values.smooth_buffer);
    }
    if touched.has("vsync") {
        o.vsync = Some(values.vsync);
    }
    if touched.has("allow_vrr") {
        o.allow_vrr = Some(values.allow_vrr);
    }
    if touched.has("overlay_actions") {
        // The whole ring, not a slot: a profile that touches it owns all of it (D10).
        o.overlay_actions = Some(values.overlay_actions.clone());
    }
    // Resets are not handled here: they clear the field and re-seed their row the moment the
    // user asks, so by the time this runs the catalog already reflects them and the row is no
    // longer marked touched.
    if let Err(e) = catalog.save() {
        tracing::warn!(error = %format!("{e:#}"), "saving the profile catalog");
    }
}

/// A compact label for a render-scale multiplier: "Native" / "1.5×" / "2× (supersample)".
fn render_scale_label(scale: f64) -> String {
    if scale == 1.0 {
        "Native".to_string()
    } else {
        // Just the multiplier: the row's caption already says what above and below 1× mean,
        // and "2× (supersample)" is long enough that the value ellipsizes to "2× (su…" the
        // moment the row grows another suffix.
        format!("{scale}×")
    }
}
const GAMEPADS: &[&str] = &[
    "auto",
    "xbox360",
    "dualsense",
    "xboxone",
    "dualshock4",
    "steamdeck",
];
/// System-button routing values (persisted under the cross-client `system_buttons` key):
/// where the guide (Xbox/PS/Steam) and quick-access presses land while streaming. Auto =
/// the host, except under Gaming Mode where the local Steam UI reacts to the same press.
const SYSTEM_BUTTONS: &[&str] = &["auto", "forward", "local"];
const SYSTEM_BUTTON_LABELS: &[&str] = &["Automatic", "Send to host", "This device"];
/// Hold-Select guide gesture values (the cross-client `guide_gesture` key). Auto arms it
/// only where the raw guide press can't reach the host (Gaming Mode here).
const GUIDE_GESTURES: &[&str] = &["auto", "on", "off"];
const GUIDE_GESTURE_LABELS: &[&str] = &["Automatic", "On", "Off"];
const COMPOSITORS: &[&str] = &["auto", "kwin", "wlroots", "mutter", "gamescope"];
/// Codec setting values (persisted) paired with their display labels below. PyroWave is
/// preference-only by design (`Settings::preferred_codec`) — the ladder falls back to
/// HEVC when either side can't do it.
const CODECS: &[&str] = &["auto", "hevc", "h264", "av1", "pyrowave"];
const CODEC_LABELS: &[&str] = &[
    "Automatic",
    "HEVC (H.265)",
    "H.264 (AVC)",
    "AV1",
    "PyroWave (wired LAN)",
];
// Stored decoder-preference values. `native-*` since M10 — the bare "vulkan"/"vaapi"
// named libavcodec's rungs, which are deleted; a store still holding them is migrated on
// read (`pf_client_core::video::migrate_decoder_pref`) and simply matches no entry here
// until the user re-picks. The labels below are unchanged and still true.
const DECODERS: &[&str] = &["auto", "native-vulkan", "native-vaapi", "software"];
/// Touch-input model values (persisted) paired with their display labels below — the
/// cross-client set (Android/Apple). Only meaningful on a touchscreen (Deck/tablet).
const TOUCH_MODES: &[&str] = &["trackpad", "pointer", "touch"];
const TOUCH_MODE_LABELS: &[&str] = &["Trackpad", "Direct pointer", "Touch passthrough"];
/// The SELECTED touch mode explained — the caption swaps with the choice (the Apple
/// revamp's dynamic-caption idiom) instead of narrating all three modes at once.
/// Combo-row captions must stay ONE line (~66 chars at the default dialog width): a
/// wrapped subtitle's natural width crushes the selected-value label into an ellipsis.
const TOUCH_MODE_CAPTIONS: &[&str] = &[
    "Drives the cursor like a laptop trackpad — tap to click",
    "The cursor jumps to your finger — a tap clicks there",
    "Real multi-touch reaches the host — for touch-native apps",
];
/// Presentation-intent values (persisted under the `present_priority` key the Apple and
/// Android clients share) + labels + dynamic captions. Captions stay ONE line, like the
/// touch/mouse rows.
const PRESENT_PRIORITIES: &[&str] = &["latency", "smooth"];
const PRESENT_PRIORITY_LABELS: &[&str] = &["Lowest latency", "Smoothness"];
const PRESENT_PRIORITY_CAPTIONS: &[&str] = &[
    "Each frame shows the moment the display can take it",
    "Buffers a little to even out network hiccups",
];
/// Smoothness buffer depth, in frames — the index IS the stored `smooth_buffer` value
/// (0 = Automatic, which resolves to 2). No millisecond hints: the cost is one refresh
/// per frame, and the session's refresh isn't known here when the mode is Native.
const SMOOTH_BUFFER_LABELS: &[&str] = &["Automatic", "1 frame", "2 frames", "3 frames"];

/// Physical-mouse model values (persisted) + labels + dynamic captions — same idiom as
/// the touch rows. Ctrl+Alt+Shift+M flips the model live in-stream.
const MOUSE_MODES: &[&str] = &["capture", "desktop"];
const MOUSE_MODE_LABELS: &[&str] = &["Capture (games)", "Desktop (absolute)"];
const MOUSE_MODE_CAPTIONS: &[&str] = &[
    "Pointer locks to the stream — relative motion, best for games",
    "Pointer moves freely in and out — best for remote desktop work",
];

/// punktfunk's own license (MIT OR Apache-2.0), shown on the About dialog's Legal page.
const APP_LICENSE: &str = concat!(
    "Punktfunk is licensed under MIT OR Apache-2.0, at your option.\n\n",
    "================================ MIT ================================\n\n",
    include_str!("../../../LICENSE-MIT"),
    "\n\n=============================== Apache-2.0 ===============================\n\n",
    include_str!("../../../LICENSE-APACHE"),
);
/// Third-party software notices for the Rust crates THIS CLIENT links — the shell, the
/// session streamer, the headless CLI and the update helper (generated by
/// scripts/gen-third-party-notices.sh; shown as a Legal section in the About dialog, and
/// shipped as /usr/share/doc/punktfunk-client/THIRD-PARTY-NOTICES.txt by the packages).
///
/// Deliberately the client-scoped file and not the workspace-wide one at the repo root:
/// that root file is the HOST's, it still carries `ffmpeg-next` and the full FFmpeg licence
/// text — and after M10 this app links no FFmpeg at all, which is exactly what the section
/// below it claims.
const THIRD_PARTY_NOTICES: &str = include_str!("../THIRD-PARTY-NOTICES.txt");

/// The dynamically linked system libraries — not in the crate notices, since they aren't
/// crates. Their full texts ship with each project rather than being vendored here.
const SYSTEM_LIBRARY_NOTICES: &str =
    "This application dynamically links system libraries under their own licenses, including \
     GTK 4 and libadwaita (LGPL v2.1+), PipeWire (MIT), and SDL 3 (Zlib). \
     Their full license texts are available from each project. Video decoding uses the \
     system's own Vulkan Video and VAAPI drivers (loaded at runtime, never linked), with \
     OpenH264 and rav1d — both BSD-2-Clause, and both in the Rust crate notices — as the \
     CPU fallback; no FFmpeg is linked or bundled.";

/// Show the About dialog (app license + the third-party-software Legal section) — reached
/// from the primary menu (app.rs `win.about`).
pub fn show_about(parent: &impl IsA<gtk::Widget>) {
    // Every licence field here is PANGO MARKUP, not plain text — so a crate author's
    // `<name@example.com>` reads as an unclosed tag and Pango drops the whole section with
    // "is not a valid name: @". The notices are generated from crate metadata and are full of
    // those, so they are escaped rather than trusted. Nothing in them wants markup anyway.
    let license = gtk::glib::markup_escape_text(APP_LICENSE);
    let crate_notices = gtk::glib::markup_escape_text(THIRD_PARTY_NOTICES);
    let system_notices = gtk::glib::markup_escape_text(SYSTEM_LIBRARY_NOTICES);
    let about = adw::AboutDialog::builder()
        .application_name("Punktfunk")
        // The app's own icon, by the id the desktop entry and the icon theme both use. It
        // resolves from the installed hicolor icon; an uninstalled dev run simply shows the
        // generic fallback rather than nothing.
        .application_icon(crate::app::APP_ID)
        .developer_name("unom")
        .version(env!("CARGO_PKG_VERSION"))
        .website("https://git.unom.io/unom/punktfunk")
        .license_type(gtk::License::Custom)
        .license(license.as_str())
        .build();
    // The native (GTK/PipeWire/SDL3) components are dynamically linked under their own
    // (LGPL/Zlib/MIT) licenses; the Rust crate notices are the substantive attribution set.
    about.add_legal_section(
        "Third-party software (Rust crates)",
        None,
        gtk::License::Custom,
        Some(crate_notices.as_str()),
    );
    about.add_legal_section(
        "Third-party software (system libraries)",
        None,
        gtk::License::Custom,
        Some(system_notices.as_str()),
    );
    about.present(Some(parent));
}

/// True inside a gamescope session (Steam game mode on the Deck / Bazzite): GTK popovers
/// are xdg_popups, which gamescope never maps for nested apps — a ComboRow's dropdown
/// flashes the row but no list ever appears. Selection UI must stay inside the toplevel.
fn gamescope_session() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.eq_ignore_ascii_case("gamescope"))
        || pf_client_core::gamescope::under_gamescope()
}

type ChangedFn = Rc<RefCell<Vec<Box<dyn Fn(u32)>>>>;

/// A titled single-choice preference row. On a desktop this is a stock popover
/// [`adw::ComboRow`]; under gamescope (see [`gamescope_session`]) it becomes an activatable
/// row that pushes an in-window selection subpage onto the preferences dialog instead.
#[derive(Clone)]
struct ChoiceRow {
    row: adw::PreferencesRow,
    selected: Rc<Cell<u32>>,
    /// Fires on user changes only — handlers are installed after seeding, so programmatic
    /// `set_selected` during setup never fires them. A list, not one: a row can carry both a
    /// dynamic caption and (in profile scope) the override mark.
    changed: ChangedFn,
    /// Subpage mode only: the current value rendered as the row's suffix.
    value_label: Option<gtk::Label>,
    options: Rc<Vec<String>>,
}

impl ChoiceRow {
    /// `inline` = subpage mode (gamescope): computed once per dialog via
    /// [`gamescope_session`] and passed in so tests can drive both modes directly.
    fn new(
        dialog: &adw::PreferencesDialog,
        inline: bool,
        title: &str,
        subtitle: &str,
        options: &[&str],
    ) -> ChoiceRow {
        let options: Rc<Vec<String>> = Rc::new(options.iter().map(|s| s.to_string()).collect());
        let selected = Rc::new(Cell::new(0u32));
        let changed: ChangedFn = Rc::new(RefCell::new(Vec::new()));

        if !inline {
            let row = adw::ComboRow::builder()
                .title(title)
                .subtitle(subtitle)
                .model(&gtk::StringList::new(
                    &options.iter().map(String::as_str).collect::<Vec<_>>(),
                ))
                .build();
            let (sel, chg) = (selected.clone(), changed.clone());
            row.connect_selected_notify(move |r| {
                if sel.replace(r.selected()) != r.selected() {
                    for f in chg.borrow().iter() {
                        f(r.selected());
                    }
                }
            });
            return ChoiceRow {
                row: row.upcast(),
                selected,
                changed,
                value_label: None,
                options,
            };
        }

        let value = gtk::Label::builder().css_classes(["dim-label"]).build();
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .activatable(true)
            .build();
        row.add_suffix(&value);
        row.add_suffix(&crate::lucide::row_icon("chevron-right"));
        {
            let dialog = dialog.downgrade();
            let (options, sel, chg, value) = (
                options.clone(),
                selected.clone(),
                changed.clone(),
                value.clone(),
            );
            let title = title.to_string();
            row.connect_activated(move |_| {
                let Some(dialog) = dialog.upgrade() else {
                    return;
                };
                let list = gtk::ListBox::builder()
                    .selection_mode(gtk::SelectionMode::None)
                    .css_classes(["boxed-list"])
                    .build();
                for (i, opt) in options.iter().enumerate() {
                    let check = crate::lucide::row_icon("check");
                    check.set_visible(i as u32 == sel.get());
                    let opt_row = adw::ActionRow::builder()
                        .title(opt)
                        .use_markup(false)
                        .activatable(true)
                        .build();
                    opt_row.add_suffix(&check);
                    let idx = i as u32;
                    let dlg = dialog.downgrade();
                    let (sel, chg, value, label) =
                        (sel.clone(), chg.clone(), value.clone(), opt.clone());
                    opt_row.connect_activated(move |_| {
                        let user_change = sel.replace(idx) != idx;
                        value.set_text(&label);
                        if user_change {
                            for f in chg.borrow().iter() {
                                f(idx);
                            }
                        }
                        if let Some(d) = dlg.upgrade() {
                            d.pop_subpage();
                        }
                    });
                    list.append(&opt_row);
                }
                let clamp = adw::Clamp::builder()
                    .child(&list)
                    .margin_top(24)
                    .margin_bottom(24)
                    .margin_start(12)
                    .margin_end(12)
                    .build();
                let scroll = gtk::ScrolledWindow::builder()
                    .hscrollbar_policy(gtk::PolicyType::Never)
                    .child(&clamp)
                    .build();
                let view = adw::ToolbarView::new();
                view.add_top_bar(&adw::HeaderBar::new());
                view.set_content(Some(&scroll));
                dialog.push_subpage(&adw::NavigationPage::new(&view, &title));
            });
        }
        let cr = ChoiceRow {
            row: row.upcast(),
            selected,
            changed,
            value_label: Some(value),
            options,
        };
        cr.sync_value();
        cr
    }

    /// Subpage mode: reflect the current selection in the row's suffix label.
    fn sync_value(&self) {
        if let Some(l) = &self.value_label {
            let i = self.selected.get() as usize;
            l.set_text(self.options.get(i).map(String::as_str).unwrap_or(""));
        }
    }

    fn widget(&self) -> &adw::PreferencesRow {
        &self.row
    }

    fn selected(&self) -> u32 {
        self.selected.get()
    }

    fn set_selected(&self, i: u32) {
        if let Some(combo) = self.row.downcast_ref::<adw::ComboRow>() {
            combo.set_selected(i); // the notify handler syncs the cell
        } else {
            self.selected.set(i);
            self.sync_value();
        }
    }

    fn connect_changed(&self, f: impl Fn(u32) + 'static) {
        self.changed.borrow_mut().push(Box::new(f));
    }
}

/// Update a row's caption after construction — the dynamic-caption hook (touch mode,
/// resolution, codec). Both ChoiceRow shapes carry their subtitle on [`adw::ActionRow`]
/// ([`adw::ComboRow`] derives from it), so one downcast covers desktop and gamescope mode.
fn set_row_subtitle(row: &adw::PreferencesRow, text: &str) {
    if let Some(r) = row.downcast_ref::<adw::ActionRow>() {
        r.set_subtitle(text);
        cap_subtitle(r);
    }
}

/// How wide a row's caption may get before it wraps. Rows put the title and subtitle in one
/// box and the control in the suffix, and that box asks for as much width as its longest line
/// wants — so a long caption starves the value label next to it, which then ellipsizes ("2×
/// (su…"). Capping the caption gives the width back to the control.
///
/// The repo's standing rule was "keep captions to one line (~66 chars)", which works until a
/// row grows another suffix — the override marker's reset button did exactly that. A cap is
/// the structural version of the same rule: it holds however many suffixes a row ends up with.
const CAPTION_CHARS: i32 = 46;

/// Cap a row's caption width. The subtitle label isn't exposed by libadwaita, so it is found by
/// matching its text — a miss simply leaves the row as it was, which is the pre-cap behaviour.
fn cap_subtitle(row: &adw::ActionRow) {
    let subtitle = row.subtitle().unwrap_or_default();
    if subtitle.is_empty() {
        return;
    }
    fn walk(w: &gtk::Widget, want: &str) -> Option<gtk::Label> {
        if let Some(l) = w.downcast_ref::<gtk::Label>() {
            if l.label() == want {
                return Some(l.clone());
            }
        }
        let mut child = w.first_child();
        while let Some(c) = child {
            if let Some(hit) = walk(&c, want) {
                return Some(hit);
            }
            child = c.next_sibling();
        }
        None
    }
    if let Some(label) = walk(row.upcast_ref(), &subtitle) {
        label.set_wrap(true);
        label.set_max_width_chars(CAPTION_CHARS);
        label.set_xalign(0.0);
        // `max_width_chars` alone only caps what the label ASKS for; an expanding label still
        // fills whatever the row hands it and goes back to one long line. Refusing the expand
        // is what actually leaves the width for the control beside it.
        label.set_hexpand(false);
        // …and Start, not the default Fill: a filled label is allocated the whole box and
        // draws on one long line regardless of the cap. Start alignment makes the allocation
        // equal the (now capped) natural width, which is what makes the cap visible.
        label.set_halign(gtk::Align::Start);
    }
}

/// Walk a page and cap every caption it contains (see [`cap_subtitle`]).
fn cap_captions(root: &gtk::Widget) {
    if let Some(row) = root.downcast_ref::<adw::ActionRow>() {
        cap_subtitle(row);
    }
    let mut child = root.first_child();
    while let Some(c) = child {
        cap_captions(&c);
        child = c.next_sibling();
    }
}

/// The SELECTED resolution choice explained (row index: 0 = Native, 1 = Match window,
/// 2.. = explicit sizes) — one line each, see the caption-width note on
/// [`TOUCH_MODE_CAPTIONS`].
fn resolution_caption(i: u32) -> &'static str {
    match i {
        0 => "The native mode of this monitor, resolved at connect",
        1 => "Follows the stream window — resizes renegotiate the host output",
        _ => "The host drives a virtual output at exactly this size",
    }
}

/// The SELECTED codec explained: the PyroWave entry is the one that needs its trade-off
/// spelled out; everything else shares the soft-preference line.
fn codec_caption(i: u32) -> &'static str {
    if CODECS.get(i as usize) == Some(&"pyrowave") {
        "Wavelet codec for wired LAN — minimal latency, lots of bandwidth"
    } else {
        "A preference — the host falls back if it can't encode it"
    }
}

/// A settings category page for the dialog's view switcher.
fn page(title: &str, icon: &str) -> adw::PreferencesPage {
    adw::PreferencesPage::builder()
        // The name addresses the page programmatically (`set_visible_page_name` — the
        // screenshot harness's page knob); the title is what the view switcher shows.
        .name(title.to_lowercase())
        .title(title)
        .icon_name(icon)
        .build()
}

/// Startup device probes for the pickers — filled by the app shell in the background
/// (GPUs via `punktfunk-session --list-adapters`, audio endpoints via the PipeWire
/// registry); any list may still be empty when the dialog opens, which simply hides
/// that picker.
#[derive(Default)]
pub struct DeviceProbes {
    pub adapters: Vec<String>,
    pub speakers: Vec<pf_client_core::audio::AudioDevice>,
    pub mics: Vec<pf_client_core::audio::AudioDevice>,
}

/// A titled group of rows; `description` (may be empty) is the one form-level note —
/// per-field explanations belong in row subtitles, not here.
fn group(title: &str, description: &str) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder().title(title).build();
    if !description.is_empty() {
        g.set_description(Some(description));
    }
    g
}

/// The dialog in a given [`Scope`]. `on_scope` asks the app to re-open it in another one:
/// switching scope closes this dialog first, so the layer being edited is committed before
/// the next one is loaded, and there is exactly one place that builds the rows.
pub fn show_scoped(
    parent: &impl IsA<gtk::Widget>,
    settings: Rc<RefCell<Settings>>,
    gamepads: &crate::gamepad::GamepadService,
    probes: &DeviceProbes,
    scope: Scope,
    on_scope: impl Fn(Scope) + 'static,
    on_closed: impl Fn() + 'static,
) -> adw::PreferencesDialog {
    let catalog = ProfilesFile::load();
    // A scope pointing at a deleted profile degrades to the defaults rather than erroring —
    // the same rule a dangling host binding follows.
    let active: Option<StreamProfile> = match &scope {
        Scope::Profile(id) => catalog.find_by_id(id).cloned(),
        Scope::Defaults => None,
    };
    let profile_mode = active.is_some();
    // Rows always show the EFFECTIVE value: the global underneath, with this profile's
    // overrides on top. A row the profile doesn't override therefore reads as the live
    // global, which is what "inherit by default" has to look like.
    let seed: Settings = match &active {
        Some(p) => p.overrides.apply(&settings.borrow()),
        None => settings.borrow().clone(),
    };
    let touched = Touched::default();
    // The globals as they are right now — what a reset row goes back to showing. Read before
    // the close handler takes ownership of the cell.
    let globals: Settings = settings.borrow().clone();
    // Where a scope switch wants to go once this dialog has committed and closed.
    let next_scope: Rc<RefCell<Option<Scope>>> = Rc::default();

    // The dialog exists before the rows: ChoiceRow's gamescope mode pushes its selection
    // subpage onto it.
    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Preferences");
    dialog.set_search_enabled(true);
    // The quick-action ring's row and editor, seeded with the scope's effective blob like
    // every row; it lives on the Input page below and reports its edits to the profile block.
    let quick = crate::ui_quick_actions::QuickActions::new(&dialog, &seed.overlay_actions);
    // Wide enough that the category switcher sits in the HEADER BAR (the tabbed look the
    // Apple/Windows clients have): AdwPreferencesDialog moves it to a bottom bar below a
    // breakpoint of 110pt × page count (≈ 733 px for our five pages). In a window that
    // can't give the dialog this width it still collapses to the bottom bar on its own.
    dialog.set_content_width(830);
    let inline = gamescope_session();

    // ---- Display: Resolution ----
    // The D1 tri-state: Native, Match window (a virtual index 1, stored as the
    // `match_window` flag), then the explicit sizes.
    let res_names: Vec<String> = std::iter::once("Native display".to_string())
        .chain(std::iter::once("Match window".to_string()))
        .chain(
            RESOLUTIONS
                .iter()
                .skip(1)
                .map(|&(w, h)| format!("{w} × {h}")),
        )
        .collect();
    let res_row = ChoiceRow::new(
        &dialog,
        inline,
        "Resolution",
        resolution_caption(0),
        &res_names.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    {
        let w = res_row.widget().clone();
        res_row.connect_changed(move |i| set_row_subtitle(&w, resolution_caption(i)));
    }
    let hz_names: Vec<String> = REFRESH
        .iter()
        .map(|&r| {
            if r == 0 {
                "Native".to_string()
            } else {
                format!("{r} Hz")
            }
        })
        .collect();
    let hz_row = ChoiceRow::new(
        &dialog,
        inline,
        "Refresh rate",
        "Native follows the monitor the window is on",
        &hz_names.iter().map(String::as_str).collect::<Vec<_>>(),
    );

    // ---- Display: Quality ----
    let scale_names: Vec<String> = RENDER_SCALES
        .iter()
        .map(|&s| render_scale_label(s))
        .collect();
    let scale_row = ChoiceRow::new(
        &dialog,
        inline,
        "Render scale",
        "Above 1× supersamples for sharpness; below is lighter on the host",
        &scale_names.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    // 1 Mbit/s per step: the rungs that matter on a thin link are 3, 4, 6 — a 5-wide step
    // could not name any of them, and typing was the only way to reach one.
    let bitrate_row = adw::SpinRow::with_range(0.0, 3000.0, 1.0);
    bitrate_row.set_title("Bitrate");
    bitrate_row
        .set_subtitle("Mbit/s · 0 = host default · a host card's menu has a network speed test");
    let codec_row = ChoiceRow::new(
        &dialog,
        inline,
        "Video codec",
        codec_caption(0),
        CODEC_LABELS,
    );
    {
        let w = codec_row.widget().clone();
        codec_row.connect_changed(move |i| set_row_subtitle(&w, codec_caption(i)));
    }
    let hdr_row = adw::SwitchRow::builder()
        .title("10-bit HDR")
        .subtitle(
            "Advertise 10-bit HDR10 so the host upgrades HDR content — shown in HDR where \
             the display supports it, tone-mapped otherwise",
        )
        .build();
    let chroma_row = adw::SwitchRow::builder()
        .title("Full chroma (4:4:4)")
        .subtitle(
            "Full-colour video: crisp small text and thin lines, at more bandwidth. HEVC \
             only, and only where the host can encode it.",
        )
        .build();
    let ten_bit_sdr_row = adw::SwitchRow::builder()
        .title("10-bit SDR")
        .subtitle(
            "Smoother gradients without HDR \u{2014} 10-bit encoding precision. Needs an \
             NVIDIA host; HDR takes over when it engages.",
        )
        .build();
    let decoder_row = ChoiceRow::new(
        &dialog,
        inline,
        "Video decoder",
        "Automatic picks the best hardware decode, then software",
        &["Automatic", "Vulkan Video", "VAAPI", "Software"],
    );
    // GPU picker (multi-GPU boxes): the adapter name feeds the session's device pick
    // via `Settings::adapter` → PUNKTFUNK_VK_ADAPTER. Hidden when there's nothing to
    // pick; a saved adapter that's gone (eGPU unplugged) keeps a revertable entry.
    let saved_adapter = seed.adapter.clone();
    let mut gpu_names = vec!["Automatic".to_string()];
    let mut gpu_keys: Vec<String> = vec![String::new()];
    for a in &probes.adapters {
        gpu_names.push(a.clone());
        gpu_keys.push(a.clone());
    }
    if !saved_adapter.is_empty() && !gpu_keys.contains(&saved_adapter) {
        gpu_names.push(format!("{saved_adapter} (not detected)"));
        gpu_keys.push(saved_adapter.clone());
    }
    let gpu_row = (gpu_keys.len() > 1).then(|| {
        let row = ChoiceRow::new(
            &dialog,
            inline,
            "GPU",
            "Decodes and presents the stream",
            &gpu_names.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let i = gpu_keys
            .iter()
            .position(|k| k == &saved_adapter)
            .unwrap_or(0);
        row.set_selected(i as u32);
        row
    });

    // ---- Display: Presentation ----
    // The intent pair the Apple and Android clients already carry. The buffer row only
    // means anything under Smoothness, so it hides itself the rest of the time rather
    // than sitting there inert.
    let present_row = ChoiceRow::new(
        &dialog,
        inline,
        "Prioritize",
        PRESENT_PRIORITY_CAPTIONS[0],
        PRESENT_PRIORITY_LABELS,
    );
    let buffer_row = ChoiceRow::new(
        &dialog,
        inline,
        "Smoothness buffer",
        "Each frame held absorbs one refresh of hiccup and adds one of delay",
        SMOOTH_BUFFER_LABELS,
    );
    {
        let w = present_row.widget().clone();
        let buffer = buffer_row.widget().clone();
        present_row.connect_changed(move |i| {
            let i = (i as usize).min(PRESENT_PRIORITY_CAPTIONS.len() - 1);
            set_row_subtitle(&w, PRESENT_PRIORITY_CAPTIONS[i]);
            buffer.set_visible(PRESENT_PRIORITIES[i] == "smooth");
        });
    }
    let vsync_row = adw::SwitchRow::builder()
        .title("V-Sync")
        .subtitle(
            "Tear-free. Turning it off removes the wait for the screen's refresh — the \
             lowest possible delay, at the cost of visible tearing. Not every driver \
             offers it; the stats overlay names the mode actually in use",
        )
        .build();
    let vrr_row = adw::SwitchRow::builder()
        .title("Follow variable refresh rate")
        .subtitle(
            "On a VRR/FreeSync/G-Sync screen, let the panel refresh in step with the \
             stream instead of on a fixed cadence. Applies to fullscreen sessions; \
             harmless on a fixed-refresh screen",
        )
        .build();

    // ---- Display: Host output ----
    let compositor_row = ChoiceRow::new(
        &dialog,
        inline,
        "Host compositor",
        "Advisory — the host falls back to auto-detect when unavailable",
        &[
            "Automatic",
            "KWin",
            "wlroots (Sway/Hyprland)",
            "Mutter (GNOME)",
            "gamescope",
        ],
    );

    // ---- General ----
    let fullscreen_row = adw::SwitchRow::builder()
        .title("Start streams in fullscreen")
        .subtitle("F11, the mouse at the top edge, or L1+R1+Start+Select lead back out")
        .build();
    let theme_row = adw::SwitchRow::builder()
        .title("Follow the Omarchy theme")
        .subtitle("Colours track omarchy-theme-set live — off keeps Punktfunk's own look")
        .build();
    let menu_row = adw::SwitchRow::builder()
        .title("Hosts in the Omarchy menu")
        .subtitle(
            "Super+Space: connect, wake, the console — this writes rows to omarchy-menu.jsonc",
        )
        .build();
    let wake_row = adw::SwitchRow::builder()
        .title("Auto-wake on connect")
        .subtitle(
            "Sends Wake-on-LAN to an offline saved host and waits for it to boot — turn \
             off if hosts behind a VPN look offline when they aren't",
        )
        .build();
    let stats_row = ChoiceRow::new(
        &dialog,
        inline,
        "Statistics overlay",
        "Compact = fps · latency · bitrate in one line — Ctrl+Alt+Shift+S cycles the tiers live",
        &["Off", "Compact", "Normal", "Detailed"],
    );
    // A spin row does both jobs the mobile pickers split across two controls: the steppers move
    // 25 % at a time (the presets), and the entry takes any percentage in range. 0 = Automatic,
    // which is 100 % on a desktop — a monitor is read at arm's length, where the design size
    // already lands. Deliberately NOT profileable: how large the chrome draws is a property of
    // this screen, not of the host a profile is authored against.
    let osd_row = adw::SpinRow::with_range(0.0, 400.0, 25.0);
    osd_row.set_title("Overlay size");
    osd_row.set_subtitle(
        "% · 0 = automatic · sizes the statistics overlay and the quick-action dial",
    );

    // ---- Input ----
    let touch_row = ChoiceRow::new(
        &dialog,
        inline,
        "Touch input",
        TOUCH_MODE_CAPTIONS[0],
        TOUCH_MODE_LABELS,
    );
    // Dynamic caption: describe the SELECTED mode, not all three at once.
    {
        let w = touch_row.widget().clone();
        touch_row.connect_changed(move |i| {
            let i = (i as usize).min(TOUCH_MODE_CAPTIONS.len() - 1);
            set_row_subtitle(&w, TOUCH_MODE_CAPTIONS[i]);
        });
    }
    let mouse_row = ChoiceRow::new(
        &dialog,
        inline,
        "Mouse input",
        MOUSE_MODE_CAPTIONS[0],
        MOUSE_MODE_LABELS,
    );
    {
        let w = mouse_row.widget().clone();
        mouse_row.connect_changed(move |i| {
            let i = (i as usize).min(MOUSE_MODE_CAPTIONS.len() - 1);
            set_row_subtitle(&w, MOUSE_MODE_CAPTIONS[i]);
        });
    }
    let inhibit_row = adw::SwitchRow::builder()
        .title("Capture system shortcuts")
        .subtitle("Forward Alt+Tab, Super, … to the host while input is captured")
        .build();
    let invert_row = adw::SwitchRow::builder()
        .title("Invert scroll direction")
        .subtitle("Reverses the wheel and trackpad scroll direction sent to the host")
        .build();

    // ---- Audio ----
    let surround_row = ChoiceRow::new(
        &dialog,
        inline,
        "Audio channels",
        "Stereo or surround — the host downmixes if its output has fewer",
        &["Stereo", "5.1 Surround", "7.1 Surround"],
    );
    let audio_format_labels: Vec<&str> = AUDIO_FORMATS.iter().map(|(_, l)| *l).collect();
    let audio_format_row = ChoiceRow::new(
        &dialog,
        inline,
        "Audio format",
        "Lossless is uncompressed PCM — 2.3–4.6 Mb/s off the top of the link, and the host has \
         its own switch",
        &audio_format_labels,
    );
    {
        // Stereo-only, and insensitive rather than hidden under surround: a lossless surround
        // frame does not fit one QUIC datagram at the default MTU, so the host declines it
        // outright (design/hi-res-audio.md §4.2). Greying it keeps the reason visible next to the
        // channel row that caused it — a row that vanished would read as a missing feature.
        //
        // Insensitivity covers the whole row including a profile scope's per-row Reset, exactly
        // as the mic-dependent rows above do: an audio_format override can only be reset while
        // the channels row says Stereo.
        let w = audio_format_row.widget().clone();
        w.set_sensitive(surround_row.selected() == 0);
        surround_row.connect_changed(move |i| w.set_sensitive(i == 0));
    }
    let keep_host_audio_row = adw::SwitchRow::builder()
        .title("Keep host audio playing")
        .subtitle("The host's speakers or headphones keep playing while you stream — needs a host on 0.32+")
        .build();
    let mic_row = adw::SwitchRow::builder()
        .title("Stream microphone")
        .subtitle("Sends your microphone to the host's virtual mic — Ctrl+Alt+Shift+V mutes it mid-stream")
        .build();
    let echo_row = adw::SwitchRow::builder()
        .title("Echo cancellation")
        .subtitle("Keeps the host's audio, playing from this machine's speakers, out of the uplink")
        .build();
    // Endpoint pickers (from the PipeWire probe): visible labels are descriptions, the
    // stored value is the node name. Hidden when the probe found nothing; a saved
    // device that's gone keeps a revertable "(not detected)" entry, like the GPU row.
    let dev_row = |saved: String,
                   devs: &[pf_client_core::audio::AudioDevice],
                   title: &str,
                   subtitle: &str| {
        let mut names = vec!["System default".to_string()];
        let mut keys = vec![String::new()];
        for d in devs {
            names.push(d.description.clone());
            keys.push(d.name.clone());
        }
        if !saved.is_empty() && !keys.contains(&saved) {
            names.push(format!("{saved} (not detected)"));
            keys.push(saved.clone());
        }
        let row = (keys.len() > 1).then(|| {
            let row = ChoiceRow::new(
                &dialog,
                inline,
                title,
                subtitle,
                &names.iter().map(String::as_str).collect::<Vec<_>>(),
            );
            row.set_selected(keys.iter().position(|k| k == &saved).unwrap_or(0) as u32);
            row
        });
        (row, keys)
    };
    let (speaker_row, speaker_keys) = dev_row(
        settings.borrow().speaker_device.clone(),
        &probes.speakers,
        "Speaker",
        "Host audio plays here — System default follows the desktop",
    );
    let (micdev_row, micdev_keys) = dev_row(
        settings.borrow().mic_device.clone(),
        &probes.mics,
        "Microphone",
        "The input that feeds the host's virtual mic",
    );
    // The device pick and the echo canceller only matter while the mic streams at all — both
    // follow it. One handler each (the pickers are optional, the echo row never is), and the
    // initial state is set here because the seed block further down fires these too.
    //
    // Insensitivity covers the whole row, including the per-row Reset a profile scope adds:
    // an echo_cancel override can only be reset while the mic row is on. Turn it on, reset,
    // turn it back off — the alternative is a control that looks live and isn't.
    if let Some(r) = &micdev_row {
        let w = r.widget().clone();
        w.set_sensitive(mic_row.is_active());
        mic_row.connect_active_notify(move |m| w.set_sensitive(m.is_active()));
    }
    {
        let w = echo_row.clone();
        w.set_sensitive(mic_row.is_active());
        mic_row.connect_active_notify(move |m| w.set_sensitive(m.is_active()));
    }

    // ---- Controllers ----
    // Controller forwarding: Automatic forwards EVERY real controller, each as its own pad
    // (Steam's virtual pad skipped); pinning one restricts the session to that single
    // controller (single-player). The pin is persisted by stable key (`Settings::forward_pad`),
    // so it survives restarts — and disconnects: an offline pinned pad keeps its entry here
    // instead of silently snapping back to Automatic.
    // Off = this device's controllers are not sent at all, because they reach the host
    // another way (USB passthrough such as VirtualHere, or a pad plugged into the host).
    // It also stops the session OPENING the pad, which is what frees the device for a
    // passthrough tool to bind — so the two rows below have nothing to act on while it is
    // off, and are desensitised to say so.
    let pad_forward_row = adw::SwitchRow::builder()
        .title("Forward controllers")
        .subtitle(
            "Send this device's controllers to the host — off if it already has them another way",
        )
        .build();
    let pads = gamepads.pads();
    let saved_pin = settings.borrow().forward_pad.clone();
    let mut pad_names = vec!["Automatic (all controllers)".to_string()];
    let mut pad_keys: Vec<String> = Vec::new();
    for p in &pads {
        let kind = p.kind_label();
        pad_names.push(if kind.is_empty() {
            p.name.clone()
        } else {
            format!("{} · {kind}", p.name)
        });
        pad_keys.push(p.key.clone());
    }
    if !saved_pin.is_empty() && !pad_keys.contains(&saved_pin) {
        let name = saved_pin
            .splitn(3, ':')
            .nth(2)
            .unwrap_or("Saved controller");
        pad_names.push(format!("{name} (not connected)"));
        pad_keys.push(saved_pin.clone());
    }
    let forward_row = ChoiceRow::new(
        &dialog,
        inline,
        "Forwarded controller",
        if pads.is_empty() {
            "No controllers detected"
        } else {
            "Every pad is its own player — pick one to force single-player"
        },
        &pad_names.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let pinned_i = pad_keys
        .iter()
        .position(|k| k == &saved_pin)
        .map_or(0, |i| i + 1);
    forward_row.set_selected(pinned_i as u32);
    // The dialog-local choice, written into Settings on close (reading the service back
    // would race its worker thread applying the Pin message).
    let chosen_pin: Rc<RefCell<String>> = Rc::new(RefCell::new(saved_pin));
    {
        let svc = gamepads.clone();
        let keys = pad_keys.clone();
        let chosen = chosen_pin.clone();
        forward_row.connect_changed(move |sel| {
            let key = if sel == 0 {
                None
            } else {
                keys.get(sel as usize - 1).cloned()
            };
            *chosen.borrow_mut() = key.clone().unwrap_or_default();
            svc.set_pinned(key);
        });
    }
    let pad_row = ChoiceRow::new(
        &dialog,
        inline,
        "Gamepad type",
        "The virtual pad on the host — Automatic matches your controller. An X-Box type has no \
         gyroscope, so pick a DualSense-class one if you want motion.",
        &[
            "Automatic",
            "Xbox 360",
            "DualSense",
            "Xbox One",
            "DualShock 4",
            "Steam Deck",
        ],
    );
    // Where the guide (Xbox/PS/Steam) + quick-access presses land, and the hold-Select
    // gesture that keeps the host's guide reachable when they stay local. Desktop rarely
    // needs either off Automatic — they exist here because profiles are authored on the
    // desktop and applied everywhere, Gaming Mode included.
    let sysbtn_row = ChoiceRow::new(
        &dialog,
        inline,
        "Steam / guide button",
        "Automatic sends it to the host, except where this device reacts to it too",
        SYSTEM_BUTTON_LABELS,
    );
    let gesture_row = ChoiceRow::new(
        &dialog,
        inline,
        "Hold Select for guide",
        "Hold Select alone for the host's guide button — a tap still goes through",
        GUIDE_GESTURE_LABELS,
    );
    // Controller audio (the 0xD1 plane): a wired DualSense's own voice coils and its little
    // built-in speaker, streamed from the host and rendered on the pad in your hands. Both are
    // negotiated — they change nothing without a capable host AND a wired DualSense — so the
    // rows say what they are for rather than promising an effect.
    //
    // Deliberately NOT profileable: which pad is in your hands is a property of this device,
    // not of the host a profile is authored against (the forwarded-pad pin below sits out for
    // the same reason).
    let haptics_row = adw::SwitchRow::builder()
        .title("Controller haptics")
        .subtitle("Play a DualSense's voice-coil haptics on the pad itself — wired pads only")
        .build();
    let pad_speaker_row = adw::SwitchRow::builder()
        .title("Controller speaker")
        .subtitle("Play the audio a game sends to the pad's own speaker on the pad, not here")
        .build();
    // The pad rows only mean something while something is being forwarded (the same
    // relationship mic → echo cancellation draws just above, initial state included: the
    // seed's `set_active` fires this only when it CHANGES the switch). Controller audio
    // belongs in that set too — forwarding off never OPENS the pad, so nothing can detect
    // that it has an audio device, let alone render on it.
    {
        let (f, t) = (forward_row.widget().clone(), pad_row.widget().clone());
        let (sb, gg) = (sysbtn_row.widget().clone(), gesture_row.widget().clone());
        let (ha, sp) = (haptics_row.clone(), pad_speaker_row.clone());
        for w in [&f, &t, &sb, &gg] {
            w.set_sensitive(seed.gamepad_forwarding);
        }
        ha.set_sensitive(seed.gamepad_forwarding);
        sp.set_sensitive(seed.gamepad_forwarding);
        pad_forward_row.connect_active_notify(move |r| {
            for w in [&f, &t, &sb, &gg] {
                w.set_sensitive(r.is_active());
            }
            ha.set_sensitive(r.is_active());
            sp.set_sensitive(r.is_active());
        });
    }

    // ---- Seed from the effective settings for this scope ----
    {
        let s = &seed;
        let res_i = index::resolution(s);
        res_row.set_selected(res_i);
        set_row_subtitle(res_row.widget(), resolution_caption(res_i));
        hz_row.set_selected(index::refresh(s));
        scale_row.set_selected(index::render_scale(s));
        bitrate_row.set_value(f64::from(s.bitrate_kbps) / 1000.0);
        pad_forward_row.set_active(s.gamepad_forwarding);
        haptics_row.set_active(s.pad_haptics);
        pad_speaker_row.set_active(pf_client_core::pad_audio::speaker_active(&s.pad_speaker));
        pad_row.set_selected(index::gamepad(s));
        sysbtn_row.set_selected(index::system_buttons(s));
        gesture_row.set_selected(index::guide_gesture(s));
        let touch_i = index::touch(s);
        touch_row.set_selected(touch_i);
        // set_selected never fires the changed hook, so seed the dynamic caption directly.
        set_row_subtitle(touch_row.widget(), TOUCH_MODE_CAPTIONS[touch_i as usize]);
        let mouse_i = index::mouse(s);
        mouse_row.set_selected(mouse_i);
        set_row_subtitle(mouse_row.widget(), MOUSE_MODE_CAPTIONS[mouse_i as usize]);
        compositor_row.set_selected(index::compositor(s));
        // Migrated for the LOOKUP only (the store is left alone): a pre-M10 settings file
        // holds `vulkan`/`vaapi`, which match no entry — the combo would show Automatic and
        // a save would silently rewrite the user's hardware preference to `auto`.
        let dec_stored = pf_client_core::video::migrate_decoder_pref(&s.decoder);
        let dec_i = DECODERS.iter().position(|&d| d == dec_stored).unwrap_or(0);
        decoder_row.set_selected(dec_i as u32);
        stats_row.set_selected(index::stats(s));
        osd_row.set_value(f64::from(punktfunk_core::osd_scale::to_percent(s.osd_scale)));
        fullscreen_row.set_active(s.fullscreen_on_stream);
        theme_row.set_active(s.follow_os_theme);
        menu_row.set_active(pf_client_core::omarchy_menu::enabled());
        wake_row.set_active(s.auto_wake);
        inhibit_row.set_active(s.inhibit_shortcuts);
        invert_row.set_active(s.invert_scroll);
        keep_host_audio_row.set_active(s.keep_host_audio);
        mic_row.set_active(s.mic_enabled);
        echo_row.set_active(s.echo_cancel);
        hdr_row.set_active(s.hdr_enabled);
        chroma_row.set_active(s.enable_444);
        ten_bit_sdr_row.set_active(s.ten_bit_sdr);
        surround_row.set_selected(index::surround(s));
        audio_format_row.set_selected(index::audio_format(s));
        // `set_selected` never fires the changed hook, so mirror the stereo gate here — the same
        // rule the smooth-buffer row's visibility follows a few lines down.
        audio_format_row
            .widget()
            .set_sensitive(index::surround(s) == 0);
        let codec_i = index::codec(s);
        codec_row.set_selected(codec_i);
        set_row_subtitle(codec_row.widget(), codec_caption(codec_i));
        let present_i = index::present_priority(s);
        present_row.set_selected(present_i);
        set_row_subtitle(
            present_row.widget(),
            PRESENT_PRIORITY_CAPTIONS[present_i as usize],
        );
        buffer_row.set_selected(index::smooth_buffer(s));
        // `set_selected` never fires the changed hook, so mirror its visibility rule here.
        buffer_row
            .widget()
            .set_visible(PRESENT_PRIORITIES[present_i as usize] == "smooth");
        vsync_row.set_active(s.vsync);
        vrr_row.set_active(s.allow_vrr);
    }

    // ---- Override markers, per-row reset, and the touch that creates an override ----
    // One pass per row, because the three are the same fact from different sides: the marker
    // says "this profile changes this", the reset is the only way back (the override model
    // never infers "not overridden" from a value comparison), and touching the control is what
    // creates it. The marker appears ON TOUCH — a user who changes a row and sees no
    // acknowledgement has no reason to believe it took.
    //
    // A reset acts IN PLACE: it clears the field, puts that one control back to the inherited
    // value, and drops the marker. Rebuilding the dialog would be simpler, but it animates the
    // whole surface for a one-row change and dumps the user back on the first page — a heavy,
    // disorienting answer to "undo this row".
    //
    // Wired after the seed block on purpose: `set_selected`/`set_active` during setup must not
    // look like the user touching anything, or opening a profile would override every row.
    if let Some(active) = &active {
        let o = &active.overrides;
        let profile_id = active.id.clone();

        // The marker pair, created LAZILY: a hidden prefix still costs its slot in the row's
        // layout, and every un-overridden row carrying an invisible dot's worth of inset reads
        // as a misalignment. Returns the "now it is overridden" switch the control's change
        // handler pulls.
        let mark = |row: &adw::PreferencesRow,
                    key: &'static str,
                    overridden: bool,
                    revert_control: Box<dyn Fn()>|
         -> Option<Rc<dyn Fn()>> {
            let row = row.downcast_ref::<adw::ActionRow>()?.clone();
            let widgets: Rc<RefCell<Option<(gtk::Box, gtk::Button)>>> = Rc::default();
            let (touched, id) = (touched.clone(), profile_id.clone());
            let revert = Rc::new(revert_control);
            let build = {
                let (widgets, row, touched, id, revert) = (
                    widgets.clone(),
                    row.clone(),
                    touched.clone(),
                    id.clone(),
                    revert.clone(),
                );
                Rc::new(move || {
                    if widgets.borrow().is_some() {
                        return;
                    }
                    let dot = gtk::Box::builder()
                        .css_classes(["pf-override-dot"])
                        .valign(gtk::Align::Center)
                        .build();
                    let reset = gtk::Button::builder()
                        .child(&crate::lucide::row_icon("undo-2"))
                        .tooltip_text("Reset to Default settings")
                        .valign(gtk::Align::Center)
                        .css_classes(["flat"])
                        .build();
                    // BOTH on the left, never in the suffix. A suffix reset shifts the row's
                    // control leftwards the instant an override appears — so the second click
                    // on a Bitrate "+" lands on a revert button that moved under the pointer,
                    // silently undoing the edit that had just been made. Nothing that appears
                    // in response to a click may displace the thing that was clicked.
                    row.add_prefix(&dot);
                    row.add_prefix(&reset);
                    {
                        let (widgets, row, touched, id, revert) = (
                            widgets.clone(),
                            row.clone(),
                            touched.clone(),
                            id.clone(),
                            revert.clone(),
                        );
                        reset.connect_clicked(move |_| {
                            // Drop the override from the catalog now — the close handler
                            // re-reads it, so there is nothing to reconcile later — and
                            // un-touch the row so the commit can't re-write what this removed.
                            touched.forget(key);
                            let mut catalog = ProfilesFile::load();
                            if let Some(p) = catalog.profiles.iter_mut().find(|p| p.id == id) {
                                p.overrides.clear(key);
                                if let Err(e) = catalog.save() {
                                    tracing::warn!(error = %format!("{e:#}"), "clearing an override");
                                }
                            }
                            revert();
                            if let Some((dot, reset)) = widgets.borrow_mut().take() {
                                row.remove(&dot);
                                row.remove(&reset);
                            }
                        });
                    }
                    *widgets.borrow_mut() = Some((dot, reset));
                }) as Rc<dyn Fn()>
            };
            if overridden {
                build();
            }
            Some(build)
        };

        // Each row: how to put it back to the INHERITED value (the globals, not this
        // profile's), and what marks it touched.
        macro_rules! choice {
            ($row:expr, $key:literal, $overridden:expr, $idx:path) => {{
                let revert = {
                    let (row, globals, touched) = ($row.clone(), globals.clone(), touched.clone());
                    Box::new(move || {
                        touched.set_suspended(true);
                        row.set_selected($idx(&globals));
                        touched.set_suspended(false);
                    }) as Box<dyn Fn()>
                };
                let show = mark($row.widget(), $key, $overridden, revert);
                let t = touched.clone();
                $row.connect_changed(move |_| {
                    if t.suspended() {
                        return;
                    }
                    t.mark($key);
                    if let Some(show) = &show {
                        show();
                    }
                });
            }};
        }
        macro_rules! toggle {
            ($row:expr, $key:literal, $overridden:expr, $field:ident) => {{
                let revert = {
                    let (row, globals, touched) = ($row.clone(), globals.clone(), touched.clone());
                    Box::new(move || {
                        touched.set_suspended(true);
                        row.set_active(globals.$field);
                        touched.set_suspended(false);
                    }) as Box<dyn Fn()>
                };
                let show = mark($row.upcast_ref(), $key, $overridden, revert);
                let t = touched.clone();
                $row.connect_active_notify(move |_| {
                    if t.suspended() {
                        return;
                    }
                    t.mark($key);
                    if let Some(show) = &show {
                        show();
                    }
                });
            }};
        }

        choice!(
            res_row,
            "resolution",
            o.width.is_some() || o.height.is_some() || o.match_window.is_some(),
            index::resolution
        );
        choice!(hz_row, "refresh_hz", o.refresh_hz.is_some(), index::refresh);
        choice!(
            scale_row,
            "render_scale",
            o.render_scale.is_some(),
            index::render_scale
        );
        choice!(codec_row, "codec", o.codec.is_some(), index::codec);
        choice!(
            compositor_row,
            "compositor",
            o.compositor.is_some(),
            index::compositor
        );
        choice!(
            stats_row,
            "stats_verbosity",
            o.stats_verbosity.is_some(),
            index::stats
        );
        choice!(
            touch_row,
            "touch_mode",
            o.touch_mode.is_some(),
            index::touch
        );
        choice!(
            mouse_row,
            "mouse_mode",
            o.mouse_mode.is_some(),
            index::mouse
        );
        choice!(
            surround_row,
            "audio_channels",
            o.audio_channels.is_some(),
            index::surround
        );
        choice!(
            audio_format_row,
            "audio_format",
            o.audio_format.is_some(),
            index::audio_format
        );
        choice!(pad_row, "gamepad", o.gamepad.is_some(), index::gamepad);
        choice!(
            sysbtn_row,
            "system_buttons",
            o.system_buttons.is_some(),
            index::system_buttons
        );
        choice!(
            gesture_row,
            "guide_gesture",
            o.guide_gesture.is_some(),
            index::guide_gesture
        );
        toggle!(
            pad_forward_row,
            "gamepad_forwarding",
            o.gamepad_forwarding.is_some(),
            gamepad_forwarding
        );
        choice!(
            present_row,
            "present_priority",
            o.present_priority.is_some(),
            index::present_priority
        );
        choice!(
            buffer_row,
            "smooth_buffer",
            o.smooth_buffer.is_some(),
            index::smooth_buffer
        );
        toggle!(vsync_row, "vsync", o.vsync.is_some(), vsync);
        toggle!(vrr_row, "allow_vrr", o.allow_vrr.is_some(), allow_vrr);
        // The ring's editor reports every edit it makes; a reset puts the inherited blob back
        // and the next open of the editor shows it.
        {
            let revert = {
                let (quick, blob) = (quick.clone(), globals.overlay_actions.clone());
                Box::new(move || quick.set_blob(&blob)) as Box<dyn Fn()>
            };
            let show = mark(
                quick.row().upcast_ref(),
                "overlay_actions",
                o.overlay_actions.is_some(),
                revert,
            );
            let t = touched.clone();
            quick.connect_changed(move || {
                t.mark("overlay_actions");
                if let Some(show) = &show {
                    show();
                }
            });
        }
        toggle!(hdr_row, "hdr_enabled", o.hdr_enabled.is_some(), hdr_enabled);
        toggle!(chroma_row, "enable_444", o.enable_444.is_some(), enable_444);
        toggle!(
            ten_bit_sdr_row,
            "ten_bit_sdr",
            o.ten_bit_sdr.is_some(),
            ten_bit_sdr
        );
        toggle!(
            fullscreen_row,
            "fullscreen_on_stream",
            o.fullscreen_on_stream.is_some(),
            fullscreen_on_stream
        );
        toggle!(
            inhibit_row,
            "inhibit_shortcuts",
            o.inhibit_shortcuts.is_some(),
            inhibit_shortcuts
        );
        toggle!(
            invert_row,
            "invert_scroll",
            o.invert_scroll.is_some(),
            invert_scroll
        );
        toggle!(
            keep_host_audio_row,
            "keep_host_audio",
            o.keep_host_audio.is_some(),
            keep_host_audio
        );
        toggle!(mic_row, "mic_enabled", o.mic_enabled.is_some(), mic_enabled);
        toggle!(
            echo_row,
            "echo_cancel",
            o.echo_cancel.is_some(),
            echo_cancel
        );
        {
            let revert = {
                let (row, globals, touched) =
                    (bitrate_row.clone(), globals.clone(), touched.clone());
                Box::new(move || {
                    touched.set_suspended(true);
                    row.set_value(f64::from(globals.bitrate_kbps) / 1000.0);
                    touched.set_suspended(false);
                }) as Box<dyn Fn()>
            };
            let show = mark(
                bitrate_row.upcast_ref(),
                "bitrate_kbps",
                o.bitrate_kbps.is_some(),
                revert,
            );
            let t = touched.clone();
            bitrate_row.connect_value_notify(move |_| {
                if t.suspended() {
                    return;
                }
                t.mark("bitrate_kbps");
                if let Some(show) = &show {
                    show();
                }
            });
        }
    }

    // ---- Assemble the category pages (the Apple revamp's map) ----
    let general = page("General", "preferences-system-symbolic");
    // The scope switcher heads the first page — the one row that is always about which layer
    // you are editing, not about the stream.
    general.add(&scope_group(
        &dialog,
        inline,
        &scope,
        &catalog,
        active.as_ref(),
        &next_scope,
        parent,
    ));
    let session_group = group("Session", "");
    session_group.add(&fullscreen_row);
    // Auto-wake is a property of the host and this network, not of "Game vs Work" — it stays
    // global in v1 (design §3, tier H/G).
    if !profile_mode {
        session_group.add(&wake_row);
    }
    // Appearance is device-level like the console's palette, never part of a profile, and
    // the row exists only where the theme does — Omarchy — rather than sitting disabled.
    if !profile_mode && pf_client_core::omarchy::present() {
        let omarchy_group = group("Omarchy", "");
        omarchy_group.add(&theme_row);
        omarchy_group.add(&menu_row);
        general.add(&omarchy_group);
    }
    let stats_group = group("Statistics", "");
    stats_group.add(stats_row.widget());
    general.add(&session_group);
    general.add(&stats_group);
    if !profile_mode {
        let overlay_group = group("Overlay", "");
        overlay_group.add(&osd_row);
        general.add(&overlay_group);
    }

    let display = page("Display", "video-display-symbolic");
    let resolution_group = group("Resolution", "");
    resolution_group.add(res_row.widget());
    resolution_group.add(hz_row.widget());
    let quality_group = group("Quality", "");
    quality_group.add(scale_row.widget());
    quality_group.add(&bitrate_row);
    quality_group.add(codec_row.widget());
    quality_group.add(&hdr_row);
    quality_group.add(&chroma_row);
    quality_group.add(&ten_bit_sdr_row);
    // Decoder and GPU are facts about THIS device's hardware — never per profile (tier G).
    if !profile_mode {
        quality_group.add(decoder_row.widget());
    }
    if let (Some(r), false) = (&gpu_row, profile_mode) {
        quality_group.add(r.widget());
    }
    let presentation_group = group("Presentation", "");
    presentation_group.add(present_row.widget());
    presentation_group.add(buffer_row.widget());
    presentation_group.add(&vsync_row);
    presentation_group.add(&vrr_row);
    // The one form-level note (deliberately not repeated on every row).
    let output_group = group(
        "Host output",
        "Display changes apply from the next session.",
    );
    output_group.add(compositor_row.widget());
    display.add(&resolution_group);
    display.add(&quality_group);
    display.add(&presentation_group);
    display.add(&output_group);

    let input = page("Input", "input-keyboard-symbolic");
    let touch_group = group("Touch", "");
    touch_group.add(touch_row.widget());
    // Group titles are Pango markup — the ampersand must be an entity.
    let kbm_group = group("Keyboard &amp; mouse", "");
    kbm_group.add(mouse_row.widget());
    kbm_group.add(&inhibit_row);
    kbm_group.add(&invert_row);
    input.add(&touch_group);
    input.add(&kbm_group);
    // The quick-action ring: one row that opens its editor — the ring itself (design
    // touch-client-overlay.md §3.3). Built with the dialog above, like every row.
    let ring_group = group(
        "Quick actions",
        "The dial a two-finger twist or Select+A opens in a stream: what its six buttons hold, \
         and the shortcut chords they can send.",
    );
    ring_group.add(quick.row());
    input.add(&ring_group);

    let audio = page("Audio", "audio-volume-high-symbolic");
    let audio_group = group("", "Applies from the next session.");
    audio_group.add(surround_row.widget());
    audio_group.add(audio_format_row.widget());
    audio_group.add(&keep_host_audio_row);
    // The speaker/mic endpoint pickers below are this device's audio routing (tier G) — they
    // render only in the defaults scope; the surround/format + mic-uplink rows above are
    // profileable.

    if let (Some(r), false) = (&speaker_row, profile_mode) {
        audio_group.add(r.widget());
    }
    audio_group.add(&mic_row);
    audio_group.add(&echo_row);
    if let (Some(r), false) = (&micdev_row, profile_mode) {
        audio_group.add(r.widget());
    }
    audio.add(&audio_group);

    let controllers = page("Controllers", "input-gaming-symbolic");
    let controllers_group = group("", "");
    // The detected-pad list (mirrors the Apple Controllers section): informational rows
    // above the pickers, from the same snapshot that feeds the forwarding picker. It is
    // about the hardware plugged into THIS device, so profile scope shows only the
    // emulated-type picker below it.
    if profile_mode {
        // nothing — the pad inventory belongs to the device, not the profile
    } else if pads.is_empty() {
        let none = adw::ActionRow::builder()
            .title("No controllers detected")
            .css_classes(["dim-label"])
            .build();
        controllers_group.add(&none);
    } else {
        for p in &pads {
            let row = adw::ActionRow::builder()
                .title(&p.name)
                .use_markup(false)
                .build();
            if p.steam_virtual {
                row.set_subtitle(
                    "Steam Input's virtual pad — Automatic skips it while a real pad is connected",
                );
            } else {
                row.set_subtitle(p.kind_label());
            }
            row.add_prefix(&crate::lucide::row_icon("gamepad-2"));
            controllers_group.add(&row);
        }
    }
    // Profileable, so it shows in both scopes — unlike the pin below it, which is about
    // which of THIS device's pads goes first: a "Work" profile can decline to forward
    // controllers to a host that a "Game" profile forwards them to.
    controllers_group.add(&pad_forward_row);
    if !profile_mode {
        controllers_group.add(forward_row.widget());
    }
    controllers_group.add(pad_row.widget());
    controllers_group.add(sysbtn_row.widget());
    controllers_group.add(gesture_row.widget());
    // Global scope only — see the rows' own note. In profile scope they would have no
    // override marker and no way to record a touch, so a toggle would be silently discarded.
    if !profile_mode {
        controllers_group.add(&haptics_row);
        controllers_group.add(&pad_speaker_row);
    }
    controllers.add(&controllers_group);

    // Cap every caption in one pass, after the rows exist: a per-row call would be sixteen
    // easy-to-forget lines, and a row added later would silently miss it.
    for page in [&general, &display, &input, &audio, &controllers] {
        cap_captions(page.upcast_ref());
    }
    dialog.add(&general);
    dialog.add(&display);
    dialog.add(&input);
    dialog.add(&audio);
    dialog.add(&controllers);

    let quick_blob = quick.blob();
    dialog.connect_closed(move |_| {
        // One reader for the rows, two destinations: the globals, or a profile's overrides.
        // Sharing it is what keeps the two scopes from interpreting the same controls
        // differently (the tri-state resolution row is the obvious trap).
        let apply_rows = |s: &mut Settings| {
            // Index 1 is the virtual "Match window" option; 0 = Native, 2.. = explicit.
            let res_i = (res_row.selected() as usize).min(RESOLUTIONS.len());
            s.match_window = res_i == 1;
            (s.width, s.height) = if res_i <= 1 {
                (0, 0)
            } else {
                RESOLUTIONS[res_i - 1]
            };
            s.refresh_hz = REFRESH[(hz_row.selected() as usize).min(REFRESH.len() - 1)];
            s.render_scale =
                RENDER_SCALES[(scale_row.selected() as usize).min(RENDER_SCALES.len() - 1)];
            // Clamps to the shared range; a typed 0 is Automatic, not a floor.
            s.osd_scale = punktfunk_core::osd_scale::from_percent(osd_row.value() as u32);
            s.bitrate_kbps = (bitrate_row.value() * 1000.0) as u32;
            // Keep a stored preference this table doesn't list (e.g. "switchpro" — valid to the
            // session, hand-edited or written by another client): it displays as "Automatic", and
            // writing that back would silently erase it just by opening + closing the dialog.
            // Persist the row only when the user picked a non-Auto entry or the stored value was
            // a listed one to begin with.
            let pad_sel = (pad_row.selected() as usize).min(GAMEPADS.len() - 1);
            if pad_sel != 0 || GAMEPADS.contains(&s.gamepad.as_str()) {
                s.gamepad = GAMEPADS[pad_sel].to_string();
            }
            s.system_buttons = SYSTEM_BUTTONS
                [(sysbtn_row.selected() as usize).min(SYSTEM_BUTTONS.len() - 1)]
            .to_string();
            s.guide_gesture = GUIDE_GESTURES
                [(gesture_row.selected() as usize).min(GUIDE_GESTURES.len() - 1)]
            .to_string();
            s.touch_mode =
                TOUCH_MODES[(touch_row.selected() as usize).min(TOUCH_MODES.len() - 1)].to_string();
            s.mouse_mode =
                MOUSE_MODES[(mouse_row.selected() as usize).min(MOUSE_MODES.len() - 1)].to_string();
            s.forward_pad = chosen_pin.borrow().clone();
            s.compositor = COMPOSITORS
                [(compositor_row.selected() as usize).min(COMPOSITORS.len() - 1)]
            .to_string();
            s.decoder =
                DECODERS[(decoder_row.selected() as usize).min(DECODERS.len() - 1)].to_string();
            if let Some(r) = &gpu_row {
                s.adapter = gpu_keys[(r.selected() as usize).min(gpu_keys.len() - 1)].clone();
            }
            if let Some(r) = &speaker_row {
                s.speaker_device =
                    speaker_keys[(r.selected() as usize).min(speaker_keys.len() - 1)].clone();
            }
            if let Some(r) = &micdev_row {
                s.mic_device =
                    micdev_keys[(r.selected() as usize).min(micdev_keys.len() - 1)].clone();
            }
            s.set_stats_verbosity(
                StatsVerbosity::ALL
                    [(stats_row.selected() as usize).min(StatsVerbosity::ALL.len() - 1)],
            );
            s.fullscreen_on_stream = fullscreen_row.is_active();
            s.follow_os_theme = theme_row.is_active();
            // Live: the switch must not wait out the shell's 2 s poll to mean something.
            crate::omarchy::set_enabled(s.follow_os_theme);
            // The menu switch is not a Settings field: the block's presence in the user's
            // omarchy-menu.jsonc IS the state, so two installs can't disagree with it.
            {
                use pf_client_core::omarchy_menu as menu;
                let want = menu_row.is_active();
                let res = match (want, menu::enabled()) {
                    (true, false) => menu::enable(),
                    (false, true) => menu::disable(),
                    _ => Ok(()),
                };
                if let Err(e) = res {
                    tracing::warn!("omarchy menu: {e}");
                }
            }
            s.auto_wake = wake_row.is_active();
            s.inhibit_shortcuts = inhibit_row.is_active();
            s.invert_scroll = invert_row.is_active();
            s.gamepad_forwarding = pad_forward_row.is_active();
            s.pad_haptics = haptics_row.is_active();
            // `"mix"` is a stored value this switch cannot express (it renders as off today,
            // pending the mixer leg), so writing the switch back unconditionally would erase
            // it just by opening and closing the dialog — the same trap the gamepad-type row
            // guards above. Only write when the user actually moved it.
            let want_speaker = pad_speaker_row.is_active();
            if want_speaker != pf_client_core::pad_audio::speaker_active(&s.pad_speaker) {
                s.pad_speaker = if want_speaker { "pad" } else { "off" }.to_string();
            }
            s.keep_host_audio = keep_host_audio_row.is_active();
            s.mic_enabled = mic_row.is_active();
            s.echo_cancel = echo_row.is_active();
            s.hdr_enabled = hdr_row.is_active();
            s.enable_444 = chroma_row.is_active();
            s.ten_bit_sdr = ten_bit_sdr_row.is_active();
            s.audio_channels = match surround_row.selected() {
                1 => 6,
                2 => 8,
                _ => 2,
            };
            // Written back whatever the channel row says. The stored choice is a preference, not
            // a live request — clearing it because the user is on 5.1 today would lose it the
            // moment they went back to stereo, and the session filters the pair anyway.
            s.audio_format = AUDIO_FORMATS
                [(audio_format_row.selected() as usize).min(AUDIO_FORMATS.len() - 1)]
            .0
            .to_string();
            s.codec = CODECS[(codec_row.selected() as usize).min(CODECS.len() - 1)].to_string();
            s.present_priority = PRESENT_PRIORITIES
                [(present_row.selected() as usize).min(PRESENT_PRIORITIES.len() - 1)]
            .to_string();
            // The index IS the value (0 = Automatic).
            s.smooth_buffer =
                (buffer_row.selected() as u8).min(SMOOTH_BUFFER_LABELS.len() as u8 - 1);
            s.vsync = vsync_row.is_active();
            s.allow_vrr = vrr_row.is_active();
            s.overlay_actions = quick_blob.borrow().clone();
        };

        match &active {
            // Profile scope writes the touched rows into the catalog and leaves the globals
            // exactly as they were — the point of the whole feature.
            Some(active) => {
                let mut values = seed.clone();
                apply_rows(&mut values);
                commit_profile(active, &touched, &values);
            }
            None => {
                // Rebase on the file, not the shell's start-of-app snapshot: the settings file
                // has other whole-file writers (the spawner persists `last_window_w/h` after a
                // match-window resize — see `profiles.rs` on why there's no merge), and saving
                // the stale snapshot here would silently revert whatever they stored while this
                // app was open. The rows carry every value this dialog owns, so applying them
                // onto a fresh load loses nothing.
                let mut s = settings.borrow_mut();
                *s = Settings::load();
                apply_rows(&mut s);
                s.save();
            }
        }
        // A scope switch closed this dialog to commit first; now re-open in the new scope.
        if let Some(next) = next_scope.borrow_mut().take() {
            on_scope(next);
        }
        on_closed();
    });
    dialog.present(Some(parent));
    dialog
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Depth-first search for an [`adw::ActionRow`] with the given title.
    fn find_action_row(root: &gtk::Widget, title: &str) -> Option<adw::ActionRow> {
        if let Some(row) = root.downcast_ref::<adw::ActionRow>() {
            if row.title() == title {
                return Some(row.clone());
            }
        }
        let mut child = root.first_child();
        while let Some(c) = child {
            if let Some(hit) = find_action_row(&c, title) {
                return Some(hit);
            }
            child = c.next_sibling();
        }
        None
    }

    fn pump() {
        let ctx = gtk::glib::MainContext::default();
        while ctx.iteration(false) {}
    }

    /// Both ChoiceRow modes in ONE test (GTK is thread-affine and libtest gives every test
    /// its own thread, so the display tests can't be split). Gamescope mode: activating the
    /// row pushes the in-window selection subpage; activating an option updates the
    /// selection + suffix label, fires the change callback, and pops the subpage. Combo
    /// mode: cell sync + change callback. Needs a display AND its own process: `--ignored` alone
    /// starts every display test in one process, and GTK refuses a second init from a second
    /// thread. Run it by name on a session box:
    /// `cargo test -p punktfunk-client-linux -- --ignored choice_row_modes`.
    #[test]
    #[ignore = "needs a Wayland/X display"]
    fn choice_row_modes() {
        assert!(gtk::init().is_ok() && adw::init().is_ok(), "no display");
        let win = adw::Window::new();
        let dialog = adw::PreferencesDialog::new();
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::new();
        let row = ChoiceRow::new(&dialog, true, "Resolution", "sub", &["A", "B", "C"]);
        group.add(row.widget());
        page.add(&group);
        dialog.add(&page);
        let fired = Rc::new(Cell::new(u32::MAX));
        {
            let f = fired.clone();
            row.connect_changed(move |i| f.set(i));
        }
        win.present();
        dialog.present(Some(&win));
        pump();

        // Suffix label reflects the seed.
        assert_eq!(row.value_label.as_ref().unwrap().text(), "A");

        // Row activation → subpage with the options list.
        row.widget()
            .downcast_ref::<adw::ActionRow>()
            .unwrap()
            .emit_by_name::<()>("activated", &[]);
        pump();
        let opt_b = find_action_row(dialog.upcast_ref(), "B").expect("subpage option missing");

        // Option activation → state + label + callback, subpage popped.
        opt_b.emit_by_name::<()>("activated", &[]);
        pump();
        assert_eq!(row.selected(), 1);
        assert_eq!(fired.get(), 1);
        assert_eq!(row.value_label.as_ref().unwrap().text(), "B");

        // The dynamic-caption hook drives the same subtitle both row shapes expose.
        set_row_subtitle(row.widget(), "swapped");
        assert_eq!(
            row.widget()
                .downcast_ref::<adw::ActionRow>()
                .unwrap()
                .subtitle()
                .as_deref(),
            Some("swapped")
        );

        // Re-activating shows the check on the new selection (fresh subpage each time).
        row.widget()
            .downcast_ref::<adw::ActionRow>()
            .unwrap()
            .emit_by_name::<()>("activated", &[]);
        pump();
        assert!(find_action_row(dialog.upcast_ref(), "B").is_some());

        // Desktop (ComboRow) mode: cell sync + change callback on selection change.
        let combo = ChoiceRow::new(&dialog, false, "Codec", "", &["X", "Y"]);
        combo.set_selected(1);
        assert_eq!(combo.selected(), 1);
        let combo_fired = Rc::new(Cell::new(u32::MAX));
        {
            let f = combo_fired.clone();
            combo.connect_changed(move |i| f.set(i));
        }
        combo.set_selected(0);
        assert_eq!(combo.selected(), 0);
        assert_eq!(combo_fired.get(), 0);
        // ComboRow derives from ActionRow, so the caption hook reaches it too.
        set_row_subtitle(combo.widget(), "combo caption");
        assert_eq!(
            combo
                .widget()
                .downcast_ref::<adw::ActionRow>()
                .unwrap()
                .subtitle()
                .as_deref(),
            Some("combo caption")
        );
    }
}
