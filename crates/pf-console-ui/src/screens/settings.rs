//! The console settings screen — the couch-relevant subset of the Settings store,
//! restyled as glass rows and fully controller-navigable (the Swift
//! `GamepadSettingsView`, re-homed): up/down moves focus, left/right steps the focused
//! value (clamped — the boundary thud tells the thumb it's the last option), A cycles
//! forward wrapping, L1/R1 change SECTION, B closes. Every change persists immediately;
//! the desktop shells read the same file, so values round-trip freely.
//!
//! The rows are split across tabs (see [`TABS`]). They used to be one 30-row scroll with
//! inline headers, which on a Deck meant thumbing past Video and Audio to reach the pad
//! settings; a tab is one shoulder press, and each tab remembers where its cursor was.
//! A tab is also one Tab keypress, and one click or tap on its pill — the strip shipped
//! reachable by shoulder buttons alone, which left it unusable to everyone holding a
//! mouse or touching the glass.

use crate::glyphs::{Hint, HintKey};
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox, Screen};
use crate::theme::{fg, Fonts, W};
use crate::widgets::{ListMsg, MenuList, RowSpec, TabStrip, TAB_STRIP_H};
use pf_client_core::audio_format::{AUDIO_FORMATS, AUDIO_FORMAT_OPUS};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use pf_client_core::trust::{MouseMode, StatsVerbosity, TouchMode};
use skia_safe::{Canvas, Rect};

/// Stable row identity — adjust/activate dispatch by id so nothing acts on a stale
/// index when the pad list under the "Use controller" row churns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowId {
    /// A catalog profile (index into [`SettingsScreen::profiles`]) — activating opens
    /// the pin-to-hosts screen. The console never edits profiles (design §5.4).
    Profile(usize),
    /// The Profiles section's placeholder while the catalog is empty.
    NoProfiles,
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
    Vsync,
    AllowVrr,
    Audio,
    /// The lossless-PCM opt-in — the cross-client `audio_format` key. Off (Opus) by default, and
    /// tied to the channel count above it for a reason that is NOT the one the design doc gives;
    /// see the `enabled` note in [`row_spec`].
    AudioFormat,
    Mic,
    EchoCancel,
    PadForward,
    Pad,
    PadType,
    SystemButtons,
    GuideGesture,
    /// The DualSense voice-coil haptics stream, rendered on the (wired) pad itself — see
    /// `trust::Settings::pad_haptics`. Negotiated: it changes nothing without a capable
    /// host and a wired DS5, which is why the row says what it is for, not what it does.
    PadHaptics,
    /// Where the pad's built-in-speaker stream renders — `trust::Settings::pad_speaker`.
    /// Offered as On (`"pad"`) / Off, exactly like the GTK switch over the same key: the
    /// third stored value (`"mix"`) is a declared TODO that renders as off, and a picker
    /// offering it would be a control that changes nothing.
    PadSpeaker,
    Touch,
    Mouse,
    InvertScroll,
    Shortcuts,
    Stats,
    Fullscreen,
    AutoWake,
    /// The gamepad UI's background colour family — see [`crate::library::PALETTES`]. The
    /// backdrop behind this very row re-colours as it steps, which is the whole reason the
    /// picker lives on a screen rather than in a dialog.
    Palette,
    /// Freeze the console's decorative motion — see `trust::Settings::reduce_motion`. Sits
    /// beside the palette row for the same reason it does: both are presentation, and the
    /// effect of stepping this one is visible on the backdrop behind it.
    ReduceMotion,
    /// Draw the console at 1080p and let the display scale it up, instead of at the panel's
    /// own resolution. Android-only, and beside [`RowId::ReduceMotion`] on purpose: both are
    /// "give up some fidelity for a smoother console", and this is the one that matters on a
    /// 4K TV or projector, where every pass the shell draws costs four times what it does at
    /// 1080p on a GPU that is not four times faster.
    ReduceUiResolution,
    /// How the game library arranges its titles — see `library::LibraryView`. The library
    /// changes it in place now, from the bar over its own field, which is where an
    /// arrangement you want to SEE the effect of belongs; this row stays because both
    /// surfaces write the one `library_view` key, so it is the same setting reached from a
    /// list, and it is where the explanation of the two arrangements lives.
    LibraryView,
    /// Whether opening a host's library lands on its collections rather than on the whole
    /// shelf — see `trust::Settings::library_collections`. Beside the view row because both
    /// answer "what does the library look like when I get there", and this one is the only
    /// way to reach the collections screen without the shelf's Y.
    LibraryCollections,
    // --- Android-only rows (design android-skia-console-port.md D3). Their values live in
    // `trust::Settings::extra` under `android.*` keys, so the desktop's settings struct never
    // grows a field one platform reads; `row_on` keeps them off the desktop's list. ---
    /// The Android decode pipeline's low-latency mode (slice-progressive delivery, DSCP).
    LowLatency,
    /// Render the host's rumble on the phone's own motor when no pad is attached.
    PhoneRumble,
    /// Send the phone's gyro as the pad's motion when no pad is attached.
    PhoneGyro,
    /// Steam Controller 2 passthrough (raw BLE/USB capture instead of the OS pad).
    Sc2Passthrough,
    /// DualSense raw-USB capture (touchpad, motion, adaptive triggers).
    DsCapture,
    /// Whether the console UI fronts the app at all — the touch settings' switch
    /// (`Settings.gamepadUiEnabled`), reachable from inside the console it turns off.
    /// Only offered where there is another interface to fall back to
    /// ([`Ctx::fallback_ui`]): on a TV or the desktop session this console is the only
    /// UI, and an off switch would strand the user in nothing.
    GamepadUi,
    /// When the console UI fronts the app: with a controller attached, or always.
    GamepadUiMode,
    /// The platform's connected-controllers view (an action row — opens a native screen).
    Controllers,
    /// The platform's open-source-licences view (an action row).
    Licenses,
}

/// The `Settings::extra` keys the Android rows read and write. Kotlin writes the same keys
/// from its own settings (`ConsoleJson.settings`) and folds them back on save.
mod android_keys {
    pub const LOW_LATENCY: &str = "android.low_latency";
    pub const PHONE_RUMBLE: &str = "android.rumble_on_phone";
    pub const PHONE_GYRO: &str = "android.gyro_on_phone";
    pub const SC2: &str = "android.sc2_capture";
    pub const DS_CAPTURE: &str = "android.ds_capture";
    pub const GAMEPAD_UI_MODE: &str = "android.gamepad_ui_mode";
    pub const GAMEPAD_UI: &str = "android.gamepad_ui_enabled";
    pub const REDUCE_UI_RES: &str = "android.reduce_ui_resolution";
}

/// The Android console-UI mode's stored values (`GamepadUi.kt`).
const GAMEPAD_UI_MODES: [(&str, &str); 2] =
    [("connected", "With a controller"), ("always", "Always")];

/// `pad_audio::speaker_active`'s answer, restated: only `"pad"` renders today ("mix" is
/// the declared TODO that renders as off). Local because that module owns the actual
/// renderer and is `cfg(linux|windows)` — this row also ships on Android, where the
/// SETTING still travels with the stream request even though no local renderer exists.
fn pad_speaker_on(mode: &str) -> bool {
    mode == "pad"
}

fn extra_bool(s: &pf_client_core::trust::Settings, key: &str, default: bool) -> bool {
    s.extra
        .get(key)
        .and_then(|v| v.as_bool())
        .unwrap_or(default)
}

fn set_extra_bool(s: &mut pf_client_core::trust::Settings, key: &str, value: bool) {
    s.extra
        .insert(key.to_string(), serde_json::Value::Bool(value));
}

fn extra_str<'a>(s: &'a pf_client_core::trust::Settings, key: &str, default: &'a str) -> &'a str {
    s.extra.get(key).and_then(|v| v.as_str()).unwrap_or(default)
}

/// `toggle` over an `extra` boolean — same clamp/wrap grammar as the typed rows.
fn toggle_extra(
    s: &mut pf_client_core::trust::Settings,
    key: &str,
    default: bool,
    delta: i32,
    wrap: bool,
) -> Option<()> {
    let mut v = extra_bool(s, key, default);
    toggle(&mut v, delta, wrap)?;
    set_extra_bool(s, key, v);
    Some(())
}

// The couch-relevant subset grew 2026-07-31: this screen is the ONLY settings editor in
// Gaming Mode, so a field it omits is simply unreachable there (render scale, 4:4:4,
// scroll/shortcut behavior, fullscreen-on-stream, auto-wake and echo cancellation all
// were). Still deliberately smaller than the desktop dialogs — device pickers
// (GPU/speaker/mic) stay desktop-only, and profiles are pinnable here (the trailing
// Profiles tab) but created and edited only in the desktop app (design §5.4).
//
// The tab names are shared with the Apple and Android gamepad settings, so a setting is
// found under the same word on every client. Profiles is the trailing tab and is built
// from the catalog at render time, which is why it carries no rows here.
const TABS: [(&str, &[RowId]); 7] = [
    (
        "Stream",
        &[
            RowId::Resolution,
            RowId::Refresh,
            RowId::RenderScale,
            RowId::Bitrate,
            RowId::Compositor,
        ],
    ),
    (
        "Video",
        &[
            RowId::Codec,
            RowId::Decoder,
            RowId::LowLatency,
            RowId::Hdr,
            RowId::Chroma444,
            RowId::PresentPriority,
            RowId::SmoothBuffer,
            RowId::Vsync,
            RowId::AllowVrr,
        ],
    ),
    (
        "Audio",
        &[
            RowId::Audio,
            RowId::AudioFormat,
            RowId::Mic,
            RowId::EchoCancel,
        ],
    ),
    (
        "Controller",
        &[
            RowId::PadForward,
            RowId::Pad,
            RowId::PadType,
            RowId::SystemButtons,
            RowId::GuideGesture,
            RowId::PadHaptics,
            RowId::PadSpeaker,
            RowId::PhoneRumble,
            RowId::PhoneGyro,
            RowId::Sc2Passthrough,
            RowId::DsCapture,
            RowId::Controllers,
        ],
    ),
    (
        "Input",
        &[
            RowId::Touch,
            RowId::Mouse,
            RowId::InvertScroll,
            RowId::Shortcuts,
        ],
    ),
    (
        "Interface",
        &[
            RowId::Palette,
            RowId::ReduceMotion,
            RowId::ReduceUiResolution,
            RowId::LibraryView,
            RowId::LibraryCollections,
            RowId::Stats,
            RowId::Fullscreen,
            RowId::AutoWake,
            RowId::GamepadUi,
            RowId::GamepadUiMode,
            RowId::Licenses,
        ],
    ),
    ("Profiles", &[]),
];

/// The index of the trailing Profiles tab (built from the catalog, not from [`TABS`]).
const PROFILES_TAB: usize = TABS.len() - 1;

/// How many sections the strip shows — for the shell's raster test, which walks all of them.
/// `cfg(test)` because nothing in a shipping build needs the count: a plain `cargo build` would
/// otherwise warn it dead, and this crate's lanes treat warnings as errors.
#[cfg(test)]
pub(crate) const TAB_COUNT: usize = TABS.len();

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
// (`punktfunk-session --browse`), where "vaapi" was a dead option that ALSO hid the real
// hardware path — `Decoder::new` has no VAAPI branch there.
//
// The STORED values are the `native-*` rung names since M10 (the libavcodec rungs those
// bare names meant are deleted). The LABELS are unchanged and still true — native Vulkan
// Video is Vulkan Video. A store written by an older client keeps working: the bare names
// migrate on read (`pf_client_core::video`'s `migrate_decoder_pref`), they just will not
// match a preset here, so the picker shows the first entry until the user re-picks.
#[cfg(not(windows))]
const DECODERS: [(&str, &str); 4] = [
    ("auto", "Automatic"),
    ("native-vulkan", "Vulkan Video"),
    ("native-vaapi", "VAAPI"),
    ("software", "Software"),
];
#[cfg(windows)]
const DECODERS: [(&str, &str); 4] = [
    ("auto", "Automatic"),
    ("native-vulkan", "Vulkan Video"),
    ("native-d3d11va", "Direct3D 11"),
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
/// Where the guide (Xbox/PS/Steam) and quick-access presses land while streaming — the
/// shared `system_buttons` key. Auto = host everywhere except Gaming Mode, where the
/// local Steam UI reacts to the same press and both overlays would open at once.
const SYSTEM_BUTTONS: [(&str, &str); 3] = [
    ("auto", "Automatic"),
    ("forward", "Send to host"),
    ("local", "This device"),
];
/// The hold-Select guide gesture — the shared `guide_gesture` key.
const GUIDE_GESTURE: [(&str, &str); 3] = [("auto", "Automatic"), ("on", "On"), ("off", "Off")];

pub(crate) struct SettingsScreen {
    list: MenuList,
    strip: TabStrip,
    /// Which of [`TABS`] is showing.
    tab: usize,
    /// Where each tab's cursor was when it was last left. Coming back to Controller after a
    /// detour through Video should land where you were, not at the top.
    tab_cursors: [usize; TABS.len()],
    /// The profile catalog's `(id, name)` pairs, loaded once at construction — the console
    /// can't create profiles (design §5.4: the desktop app does), so the list is stable
    /// for the screen's lifetime.
    profiles: Vec<(String, String)>,
}

impl SettingsScreen {
    /// The settings screen over the host's profile catalog (`store.profiles()`), read once
    /// here — the console can't create profiles, so the list is stable for the screen's
    /// lifetime.
    pub(crate) fn new(store: &dyn crate::store::SettingsStore) -> SettingsScreen {
        Self::with_profiles(store.profiles())
    }

    fn with_profiles(profiles: Vec<(String, String)>) -> SettingsScreen {
        SettingsScreen {
            list: MenuList::new(),
            strip: TabStrip::new(),
            tab: 0,
            tab_cursors: [0; TABS.len()],
            profiles,
        }
    }

    /// The rows of the CURRENT tab, minus any whose setting has nothing to act on (see
    /// [`row_applies`]). Profiles is built from the catalog: one row per profile, or the
    /// explainer placeholder while there are none.
    fn row_ids(&self, ctx: &Ctx) -> Vec<RowId> {
        if self.tab != PROFILES_TAB {
            return TABS[self.tab]
                .1
                .iter()
                .copied()
                .filter(|id| row_on(*id, ctx.platform) && row_applies(*id, ctx))
                .collect();
        }
        if self.profiles.is_empty() {
            vec![RowId::NoProfiles]
        } else {
            (0..self.profiles.len()).map(RowId::Profile).collect()
        }
    }

    /// Pull the cursor back onto the list. Every tab but Profiles used to be a fixed length,
    /// so this only mattered on entry ([`show_tab`]); the smoothness buffer's row now comes
    /// and goes, and another writer (a desktop shell, a session's match-window persist) can
    /// take it away between frames while this screen is open.
    fn clamp_cursor(&mut self, len: usize) {
        if self.list.cursor >= len {
            self.list.jump_to(len.saturating_sub(1));
        }
    }

    #[cfg(test)]
    pub(crate) fn tab_for_test(&self) -> usize {
        self.tab
    }

    /// Row `i`'s rect as last drawn — the shell's touch tests press real coordinates.
    #[cfg(test)]
    pub(crate) fn row_rect_for_test(&self, i: usize) -> Option<Rect> {
        self.list.row_rect(i)
    }

    /// L1/R1 (and Tab/PgUp/PgDn) — move one tab, wrapping (the strip is a ring, like A's
    /// value cycle), keeping each tab's own cursor.
    fn switch_tab(&mut self, delta: i32, ctx: &Ctx) -> Option<MenuPulse> {
        let n = TABS.len() as i32;
        self.show_tab((self.tab as i32 + delta).rem_euclid(n) as usize, ctx)
    }

    /// Show `tab`, parking the cursor the outgoing tab was on. Also the pointer's path in:
    /// a press on a pill names a tab outright rather than a direction to step in.
    fn show_tab(&mut self, tab: usize, ctx: &Ctx) -> Option<MenuPulse> {
        if tab >= TABS.len() {
            return None;
        }
        self.tab_cursors[self.tab] = self.list.cursor;
        self.tab = tab;
        // Clamp the remembered cursor: the Profiles tab's length follows the catalog, and
        // Video's follows whether the smoothness buffer is offered.
        let len = self.row_ids(ctx).len();
        self.list
            .jump_to(self.tab_cursors[self.tab].min(len.saturating_sub(1)));
        Some(MenuPulse::Move)
    }

    /// Mouse/touch. The strip is checked first — its pills sit above the list and a press
    /// there is never meant for a row.
    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        if let Some(tab) = self.strip.pointer(p) {
            self.show_tab(tab, ctx);
            return true;
        }
        let ids = self.row_ids(ctx);
        self.clamp_cursor(ids.len());
        let (msg, pulse) = self.list.pointer(p, ids.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.apply_row(msg, pulse, &ids, ctx, fx);
        true
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        match ev {
            MenuEvent::Back => {
                fx.pop();
                return None;
            }
            MenuEvent::JumpBack => return self.switch_tab(-1, ctx),
            MenuEvent::JumpForward => return self.switch_tab(1, ctx),
            _ => {}
        }
        let ids = self.row_ids(ctx);
        self.clamp_cursor(ids.len());
        let (msg, pulse) = self.list.menu(ev, ids.len());
        self.apply_row(msg, pulse, &ids, ctx, fx)
    }

    /// What a list message means on the focused row — shared by the pad/keyboard path and
    /// the pointer's, so a click and an A press can never drift apart.
    fn apply_row(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        ids: &[RowId],
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        // A cursor with no row under it can only mean the list shrank between the clamp above
        // and here, which nothing does today — but indexing on the assumption would turn that
        // into a panic in a shipping console rather than a dropped keypress.
        let Some(&focused) = ids.get(self.list.cursor) else {
            return pulse;
        };
        // The Profiles rows navigate instead of editing the settings file.
        match focused {
            RowId::Profile(i) => {
                return match msg {
                    ListMsg::Activate => {
                        let (id, name) = self.profiles[i].clone();
                        fx.push(Screen::PinHosts(super::pin_hosts::PinHostsScreen::new(
                            id, name,
                        )));
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            RowId::NoProfiles => {
                return match msg {
                    ListMsg::Adjust(_) | ListMsg::Activate => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            // Connected controllers is one of ours now — a shared Skia screen, so the console
            // keeps its own input on the page and only the grant dialogs go back to the host.
            RowId::Controllers => {
                return match msg {
                    ListMsg::Activate => {
                        fx.push(Screen::Controllers(
                            super::controllers::ControllersScreen::new(),
                        ));
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            // The one screen still the platform's: A asks the host to open it; nothing here
            // edits.
            RowId::Licenses => {
                return match msg {
                    ListMsg::Activate => {
                        fx.cmds.push(crate::model::ConsoleCmd::OpenPlatformScreen {
                            id: crate::platform::PlatformScreen::Licenses.id().to_string(),
                        });
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            _ => {}
        }
        // Rebase the shell-lifetime snapshot on the file before an adjust-then-save: this
        // screen is one of the settings file's several whole-file writers (profiles.rs
        // documents the no-merge debt), and adjusting a stale snapshot would silently
        // revert what another writer (a session's match-window persist, a desktop shell)
        // stored while the console was open. Only on the mutating events — a cursor move
        // shouldn't touch the disk.
        if matches!(msg, ListMsg::Adjust(_) | ListMsg::Activate) {
            *ctx.settings = ctx.store.load();
        }
        match msg {
            ListMsg::Adjust(delta) => {
                let changed = adjust(focused, delta, false, ctx);
                if changed {
                    ctx.store.save(ctx.settings);
                    Some(MenuPulse::Move)
                } else {
                    Some(MenuPulse::Boundary)
                }
            }
            ListMsg::Activate => {
                // A cycles forward WRAPPING, so every option is reachable one-handed.
                if adjust(focused, 1, true, ctx) {
                    ctx.store.save(ctx.settings);
                }
                pulse
            }
            ListMsg::None => pulse,
        }
    }

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        let ids = self.row_ids(ctx);
        // The shoulders always change section, so that hint leads on every row.
        let mut hints = vec![Hint::new(HintKey::Shoulders, "Section")];
        hints.extend(match ids.get(self.list.cursor) {
            Some(RowId::Profile(_)) => vec![
                Hint::new(HintKey::Confirm, "Pin to hosts…"),
                Hint::new(HintKey::Back, "Done"),
            ],
            Some(RowId::NoProfiles) | None => vec![Hint::new(HintKey::Back, "Done")],
            Some(RowId::Controllers | RowId::Licenses) => vec![
                Hint::new(HintKey::Confirm, "Open"),
                Hint::new(HintKey::Back, "Done"),
            ],
            Some(_) => vec![
                Hint::new(HintKey::Adjust, "Adjust"),
                Hint::new(HintKey::Confirm, "Change"),
                Hint::new(HintKey::Back, "Done"),
            ],
        });
        hints
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
        // The tab strip takes the top band, the focused row's explainer a reserved band
        // under the list; the rows get what's between.
        let detail_h = 34.0 * k;
        let strip_h = TAB_STRIP_H * k;
        let labels: Vec<&str> = TABS.iter().map(|(name, _)| *name).collect();
        self.strip.render(
            canvas,
            Rect::from_ltrb(rect.left, rect.top, rect.right, rect.top + strip_h as f32),
            &labels,
            self.tab,
            fonts,
            k,
            dt,
        );
        let list_rect = Rect::from_ltrb(
            rect.left,
            rect.top + strip_h as f32,
            rect.right,
            rect.bottom - detail_h as f32,
        );
        let ids = self.row_ids(ctx);
        self.clamp_cursor(ids.len());
        let rows: Vec<RowSpec> = ids
            .iter()
            .map(|id| row_spec(*id, ctx, &self.profiles))
            .collect();
        self.list
            .render(canvas, list_rect, &rows, fonts, k, dt, true);
        let detail = ids
            .get(self.list.cursor)
            .copied()
            .map_or("", |id| detail(id, ctx.platform));
        fonts.centered(
            canvas,
            detail,
            W::Regular,
            13.0 * k,
            fg(0.55),
            f64::from(rect.left) + f64::from(rect.width()) / 2.0,
            f64::from(rect.bottom) - detail_h + 6.0 * k,
            f64::from(rect.width()) * 0.8,
        );
    }
}

/// Whether a row is OFFERED at all, as opposed to offered-but-inert.
///
/// The two are a real distinction. Echo cancellation and the pad rows follow a switch the user
/// can see a line or two above them, so dimming them shows the relationship — dropping them
/// would just make settings appear and disappear as the switch flips. The smoothness buffer is
/// different: it is not a sub-setting of a switch, it is a knob on ONE of two intents, and
/// under Lowest latency it names a quantity that doesn't exist. Every other settings surface —
/// the GTK and WinUI shells, the Apple touch/tvOS screens, the Android touch screen — hides it
/// there. This screen was the lone exception because its row list was fixed; it is rebuilt from
/// this filter each frame now, and the row it drops sits directly BELOW the row that drops it,
/// so the cursor is never under anything that moves.
/// Which platform HAS a row (design android-skia-console-port.md D3). The tables above are
/// one union so a setting is found under the same word on every client; a row that names a
/// concept the platform does not have — a decoder picker, vsync, fullscreen-on-stream, the
/// keyboard-shortcut grab — is simply absent there, never a control that changes nothing.
/// The desktop shows everything, exactly as before there was a second platform.
fn row_on(id: RowId, platform: crate::platform::Platform) -> bool {
    use crate::platform::Platform;
    let android_only = matches!(
        id,
        RowId::LowLatency
            | RowId::PhoneRumble
            | RowId::PhoneGyro
            | RowId::Sc2Passthrough
            | RowId::DsCapture
            | RowId::GamepadUi
            | RowId::GamepadUiMode
            | RowId::ReduceUiResolution
            | RowId::Controllers
            | RowId::Licenses
    );
    let desktop_only = matches!(
        id,
        RowId::Decoder
            | RowId::Chroma444
            | RowId::Vsync
            | RowId::AllowVrr
            | RowId::Fullscreen
            | RowId::Shortcuts
    );
    match platform {
        Platform::Desktop => !android_only,
        Platform::Android => !desktop_only,
    }
}

fn row_applies(id: RowId, ctx: &Ctx) -> bool {
    match id {
        RowId::SmoothBuffer => ctx.settings.present_priority == "smooth",
        // The console-off switch needs somewhere for "off" to land: only clients with a
        // fallback interface (an Android phone/tablet's touch shell) get the row — on a TV
        // this console is the only UI, and off would strand the user (the touch settings'
        // subtitle even promises "A TV always uses it").
        RowId::GamepadUi => ctx.fallback_ui,
        // The same two conditions the mode decides anything under: a TV is in console mode
        // whatever the mode says (`GamepadUi.kt`: the tv term alone satisfies the OR), and
        // while the switch above is off nothing fronts the console at all. Hidden rather
        // than dimmed, like the touch screen's picker, and it sits directly below the row
        // that drops it so the cursor is never under anything that moves.
        RowId::GamepadUiMode => {
            ctx.fallback_ui && extra_bool(ctx.settings, android_keys::GAMEPAD_UI, true)
        }
        _ => true,
    }
}

fn row_spec(id: RowId, ctx: &Ctx, profiles: &[(String, String)]) -> RowSpec {
    // The Profiles section: name + how many hosts pin it (counted from the live rows, so
    // it reflects what the carousel shows). Read-only here beyond opening the pin screen.
    match id {
        RowId::Profile(i) => {
            let (pid, name) = &profiles[i];
            let pins = ctx
                .hosts
                .iter()
                .filter(|h| h.pin.as_ref().is_some_and(|p| &p.id == pid))
                .count();
            return RowSpec {
                header: None,
                label: name.clone(),
                value: Some(match pins {
                    0 => "Not pinned".into(),
                    1 => "Pinned to 1 host".into(),
                    n => format!("Pinned to {n} hosts"),
                }),
                value_dim: pins == 0,
                caret: false,
                adjustable: false,
                enabled: true,
            };
        }
        RowId::NoProfiles => {
            return RowSpec::action("No profiles yet", false);
        }
        RowId::Controllers => return RowSpec::action("Connected controllers", true),
        RowId::Licenses => return RowSpec::action("Open-source licences", true),
        _ => {}
    }
    let s = &ctx.settings;
    // Three rows follow a switch a line or two above them: echo cancellation only means
    // anything while the mic streams, the audio format only while the stream is stereo, and
    // the pad rows only while any controller is forwarded at all. All go dim and inert
    // otherwise — the same relationship the desktop shells draw by greying a row out, and
    // dimming (not dropping) is what shows the relationship. The smoothness buffer used to be
    // listed here too; it is dropped from the list instead now — see [`row_applies`] for why
    // that one is different.
    let enabled = match id {
        RowId::EchoCancel => s.mic_enabled,
        // ⚠ Lossless follows the channel count for a reason that has MOVED, and the old reason
        // is still written down in several places that are now wrong (`hi-res-audio.md` §4.2's
        // blanket "surround does not fit a datagram", and `trust::Settings::audio_format`'s doc
        // quoting it). The HOST's `channels != 2` decline is deleted: `pcm::frame_us_for` is
        // channel-aware, so 48 kHz/24-bit 5.1 and 7.1 simply negotiate a SHORTER frame and fit
        // an ordinary datagram — only 96 kHz surround has nowhere left on the ladder. The Apple
        // and Android console screens dropped their gates on the strength of that, and this row
        // would too if it could.
        //
        // It cannot, yet: `pf_client_core::session` still filters `audio_channels != 2` out of
        // the request BEFORE it reaches the wire, so a lossless choice made here under 5.1/7.1
        // is never even asked for — the session logs "lossless audio is stereo-only" and runs
        // Opus. A live row would be a control that changes nothing, which is exactly the lie
        // this screen dims rows to avoid, and it would also disagree with the GTK dialog
        // reading the same settings file on this same machine. Delete this arm when that
        // client-side filter learns the frame ladder — not before.
        RowId::AudioFormat => s.audio_channels == 2,
        RowId::Pad
        | RowId::PadType
        | RowId::SystemButtons
        | RowId::GuideGesture
        | RowId::PadHaptics
        | RowId::PadSpeaker => s.gamepad_forwarding,
        _ => true,
    };
    let (header, label, value): (Option<&'static str>, &str, String) = match id {
        RowId::Resolution => (
            None,
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
        RowId::Codec => (None, "Video codec", label_for(&CODECS, &s.codec).into()),
        // Migrated on the way in: a pre-M10 store holds `vulkan`/`vaapi`/`d3d11va`,
        // which name no preset here and would otherwise render as "—".
        RowId::Decoder => (
            None,
            "Decoder",
            label_for(
                &DECODERS,
                &pf_client_core::decoder_pref::migrate_decoder_pref(&s.decoder),
            )
            .into(),
        ),
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
        RowId::Vsync => (None, "V-Sync", on_off(s.vsync).into()),
        RowId::AllowVrr => (None, "Follow variable refresh", on_off(s.allow_vrr).into()),
        RowId::Audio => (
            None,
            "Audio channels",
            AUDIO
                .iter()
                .find(|(v, _)| *v == s.audio_channels)
                .map_or("Stereo", |(_, l)| l)
                .into(),
        ),
        RowId::AudioFormat => (
            None,
            "Audio quality",
            audio_format_label(&s.audio_format).into(),
        ),
        RowId::Mic => (None, "Microphone", on_off(s.mic_enabled).into()),
        RowId::EchoCancel => (None, "Echo cancellation", on_off(s.echo_cancel).into()),
        RowId::PadForward => (
            None,
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
        RowId::SystemButtons => (
            None,
            "Steam / guide button",
            label_for(&SYSTEM_BUTTONS, &s.system_buttons).into(),
        ),
        RowId::GuideGesture => (
            None,
            "Hold Select for guide",
            label_for(&GUIDE_GESTURE, &s.guide_gesture).into(),
        ),
        RowId::PadHaptics => (None, "Controller haptics", on_off(s.pad_haptics).into()),
        RowId::PadSpeaker => (
            None,
            "Controller speaker",
            on_off(pad_speaker_on(&s.pad_speaker)).into(),
        ),
        RowId::Touch => (None, "Touch mode", s.touch_mode().label().into()),
        RowId::Mouse => (None, "Mouse mode", s.mouse_mode().label().into()),
        RowId::InvertScroll => (None, "Invert scroll", on_off(s.invert_scroll).into()),
        RowId::Shortcuts => (
            None,
            "Capture system shortcuts",
            on_off(s.inhibit_shortcuts).into(),
        ),
        RowId::Palette => (
            None,
            "Background",
            crate::library::palette(&s.ui_palette).name.into(),
        ),
        // Phrased as the thing that is ON, not as the suppression, so "On" means the
        // reduction is in effect — the same way every other toggle on this screen reads.
        RowId::ReduceMotion => (None, "Reduce motion", on_off(s.reduce_motion).into()),
        RowId::ReduceUiResolution => (
            None,
            "Reduce interface resolution",
            on_off(extra_bool(s, android_keys::REDUCE_UI_RES, false)).into(),
        ),
        RowId::LibraryView => (
            None,
            "Library view",
            crate::library::LibraryView::parse(&s.library_view)
                .label()
                .into(),
        ),
        RowId::LibraryCollections => (
            None,
            "Start in collections",
            on_off(s.library_collections).into(),
        ),
        RowId::Stats => (
            None,
            "Statistics overlay",
            s.stats_verbosity().label().into(),
        ),
        RowId::Fullscreen => (
            None,
            "Start streams fullscreen",
            on_off(s.fullscreen_on_stream).into(),
        ),
        RowId::AutoWake => (None, "Wake hosts automatically", on_off(s.auto_wake).into()),
        RowId::LowLatency => (
            Some("Decoding"),
            "Low-latency mode",
            on_off(extra_bool(s, android_keys::LOW_LATENCY, true)).into(),
        ),
        RowId::PhoneRumble => (
            Some("This device"),
            "Rumble on this phone",
            on_off(extra_bool(s, android_keys::PHONE_RUMBLE, false)).into(),
        ),
        RowId::PhoneGyro => (
            None,
            "Gyro from this phone",
            on_off(extra_bool(s, android_keys::PHONE_GYRO, false)).into(),
        ),
        RowId::Sc2Passthrough => (
            Some("Passthrough"),
            "Steam Controller 2",
            on_off(extra_bool(s, android_keys::SC2, true)).into(),
        ),
        RowId::DsCapture => (
            None,
            "DualSense over USB",
            on_off(extra_bool(s, android_keys::DS_CAPTURE, true)).into(),
        ),
        RowId::GamepadUi => (
            None,
            "Controller-optimized UI",
            on_off(extra_bool(s, android_keys::GAMEPAD_UI, true)).into(),
        ),
        RowId::GamepadUiMode => (
            None,
            // The touch screen's word for the same picker, which now sits under the same
            // switch it does there — "Controller UI" beside "Controller-optimized UI"
            // would be two rows a reader has to tell apart by their tails.
            "Show it",
            label_for(
                &GAMEPAD_UI_MODES,
                extra_str(s, android_keys::GAMEPAD_UI_MODE, "connected"),
            )
            .into(),
        ),
        RowId::Profile(_) | RowId::NoProfiles | RowId::Controllers | RowId::Licenses => {
            unreachable!("returned above")
        }
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

/// The focused row's one-line explainer. Takes the platform because two desktop rows
/// advertise desktop-only live chords (Ctrl+Alt+Shift+…) that no Android build has — a
/// shortcut the device cannot press must not be taught.
fn detail(id: RowId, platform: crate::platform::Platform) -> &'static str {
    use crate::platform::Platform;
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
        RowId::Decoder => "Automatic picks the best hardware decoder for this GPU, then software.",
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
        RowId::Vsync => {
            "Tear-free. Off removes the wait for the screen's refresh — the lowest \
             possible delay, at the cost of visible tearing. Not every driver offers it; \
             the stats overlay names the mode actually in use."
        }
        RowId::AllowVrr => {
            "On a VRR screen, let the panel refresh in step with the stream instead of on \
             a fixed cadence. Applies to fullscreen sessions; harmless on a fixed screen."
        }
        RowId::Audio => "The speaker layout requested from the host.",
        RowId::AudioFormat => {
            "Bit-exact PCM instead of Opus — 2.3 Mb/s at 48 kHz, 4.6 at 96, off the top of the \
             link. The host has its own switch and stays on Opus if it can't deliver the rate; \
             the stats overlay names what the session got. Stereo only."
        }
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
        RowId::SystemButtons => {
            "Where the guide (Xbox/PS/Steam) and quick-access presses go. Automatic \
             sends them to the host except in Gaming Mode, where Steam on this device \
             reacts to the same press and both overlays would open at once."
        }
        RowId::GuideGesture => {
            "Hold Select on its own to press the host's guide button — keep holding for \
             the host's quick-access menu. Automatic arms it only where the real button \
             can't reach the host. A Select tap still goes through, slightly delayed."
        }
        RowId::PadHaptics => {
            "Play a DualSense's fine-grained haptics on the pad itself instead of plain \
             rumble. Negotiated — it changes nothing without a capable host and a wired pad."
        }
        RowId::PadSpeaker => {
            "Play the audio a game sends to the controller's own speaker on the pad, \
             not through this device's output."
        }
        RowId::Touch => {
            "How the touchscreen drives the host: Trackpad (relative cursor), \
             Direct pointer (cursor jumps to your finger), or Touch passthrough (raw contacts)."
        }
        RowId::Mouse => match platform {
            Platform::Desktop => {
                "How a physical mouse drives the host: Capture locks the pointer (relative, \
                 for games), Desktop leaves it free and sends absolute positions. \
                 Ctrl+Alt+Shift+M switches live while streaming."
            }
            Platform::Android => {
                "How a physical mouse drives the host: Capture locks the pointer (relative, \
                 for games), Desktop leaves it free and sends absolute positions."
            }
        },
        RowId::InvertScroll => "Reverses the wheel and trackpad scroll direction sent to the host.",
        RowId::Shortcuts => {
            "Alt+Tab, Super and friends reach the host while input is captured. \
             Off, they act on this device instead."
        }
        RowId::Palette => {
            "The colour family this backdrop drifts through — it changes as you step, so \
             pick by looking. Appearance only; nothing about a stream depends on it."
        }
        RowId::ReduceMotion => {
            "Freezes the backdrop and replaces the console's slides and pops with plain \
             fades. Also the gentler choice on an OLED, where a still field can sit for \
             hours."
        }
        RowId::ReduceUiResolution => {
            "Draws the menus at 1080p and lets the display scale them up. Text goes a \
             little softer; the console gets much smoother on a 4K TV or projector, whose \
             graphics chip is far slower than the panel in front of it. Nothing about a \
             stream changes — this is the interface only."
        }
        RowId::LibraryView => {
            "Shelf shows one cover at a time, big. Grid shows about eighteen at once — \
             for when you already know what you are looking for. The library's own bar \
             switches it while you browse, along with the sort."
        }
        RowId::LibraryCollections => {
            "Opening a host's library goes straight to its collections — platforms and \
             stores as tiles — instead of the whole shelf. A library with only one \
             collection opens on the shelf as usual."
        }
        RowId::Stats => match platform {
            Platform::Desktop => {
                "How much the overlay shows: Compact (one line) → Normal → Detailed. \
                 Ctrl+Alt+Shift+S cycles it live while streaming."
            }
            Platform::Android => {
                "How much the overlay shows: Compact (one line) → Normal → Detailed."
            }
        },
        RowId::Fullscreen => "Streams open fullscreen instead of windowed.",
        RowId::AutoWake => {
            "Send Wake-on-LAN to a sleeping host before connecting. Turn off for hosts \
             reached over a VPN, where the wake wait only adds delay."
        }
        RowId::LowLatency => {
            "Feeds the decoder slice by slice as frames arrive and marks the media sockets \
             for priority. Off if a decoder shows artefacts under it."
        }
        RowId::PhoneRumble => {
            "Play the host's rumble on this phone's own motor when no controller is \
             attached. Costs battery; does nothing with a pad connected."
        }
        RowId::PhoneGyro => {
            "Send this phone's motion as the controller's gyro when no controller is \
             attached. Only games that read gyro notice; a pad's own gyro wins."
        }
        RowId::Sc2Passthrough => {
            "Capture the Steam Controller 2 directly (touchpads, gyro, paddles) instead of \
             the generic pad Android shows. Needs the Bluetooth or USB grant."
        }
        RowId::DsCapture => {
            "Capture a wired DualSense directly (touchpad, motion, adaptive triggers). \
             Needs the USB grant when the pad is plugged in."
        }
        RowId::GamepadUi => {
            "Front the app with this console instead of the touch interface. Off returns \
             to the touch home immediately — switch it back on there."
        }
        RowId::GamepadUiMode => {
            "When this console fronts the app: whenever a controller is attached, or \
             always — for a device that lives docked to a TV. The switch above turns it \
             off altogether."
        }
        RowId::Controllers => "Connected controllers, their grants and a rumble/haptics test.",
        RowId::Licenses => "The open-source licences this app ships under.",
        RowId::Profile(_) => {
            "Pin this profile to a host and it appears as its own card — one press \
             connects with these settings. Profiles are created and edited in the \
             Punktfunk desktop app."
        }
        RowId::NoProfiles => {
            "Profiles bundle stream settings for different uses (a low-latency one, a \
             quality one…). Create them in the Punktfunk desktop app, then pin them \
             here as one-press connect cards."
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

/// The label for a stored `audio_format` value — [`label_for`] with a different miss, on purpose.
///
/// An unrecognized value is not a corrupt one here: the key travels verbatim through a profile
/// catalog shared with the Apple and Android clients, so a rung a NEWER client offers can land in
/// this file. The session resolves anything it doesn't know to Opus
/// (`pf_client_core::audio_format::audio_format_wire`), so the row says Opus too. `label_for`'s "—"
/// would name a format no session on this box will ever run.
fn audio_format_label(value: &str) -> &'static str {
    AUDIO_FORMATS
        .iter()
        .find(|(v, _)| *v == value)
        .or_else(|| AUDIO_FORMATS.iter().find(|(v, _)| *v == AUDIO_FORMAT_OPUS))
        .map_or("", |(_, l)| *l)
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
        RowId::Decoder => {
            // …and on the way in here too, or stepping from a legacy value would start
            // from "not found" and jump to the first/last entry instead of the neighbour
            // of what the user actually has.
            s.decoder = pf_client_core::decoder_pref::migrate_decoder_pref(&s.decoder);
            step_str(&DECODERS, &mut s.decoder, delta, wrap)
        }
        RowId::Hdr => toggle(&mut s.hdr_enabled, delta, wrap),
        RowId::Chroma444 => toggle(&mut s.enable_444, delta, wrap),
        RowId::PresentPriority => {
            let cur = PRESENT_PRIORITIES
                .iter()
                .position(|(v, _)| *v == s.present_priority);
            step_option(cur, PRESENT_PRIORITIES.len(), delta, wrap)
                .map(|i| s.present_priority = PRESENT_PRIORITIES[i].0.to_string())
        }
        // Under Lowest latency the row isn't offered at all ([`row_applies`]), so this branch
        // is only reachable if another writer flipped the intent between the frame that built
        // the list and the keypress that lands here — a boundary thud, not a stored value
        // nothing will read.
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
        RowId::Vsync => toggle(&mut s.vsync, delta, wrap),
        RowId::AllowVrr => toggle(&mut s.allow_vrr, delta, wrap),
        RowId::Audio => {
            let cur = AUDIO.iter().position(|(v, _)| *v == s.audio_channels);
            step_option(cur, AUDIO.len(), delta, wrap).map(|i| s.audio_channels = AUDIO[i].0)
        }
        // Inert under surround — a boundary thud, matching what the dimmed row shows. The gate is
        // this client's own request filter rather than anything about the plane; the `enabled`
        // note in `row_spec` is where that is written down, and where it gets deleted.
        RowId::AudioFormat => {
            if s.audio_channels == 2 {
                step_str(AUDIO_FORMATS, &mut s.audio_format, delta, wrap)
            } else {
                None
            }
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
        RowId::SystemButtons => {
            if !s.gamepad_forwarding {
                return false;
            }
            step_str(&SYSTEM_BUTTONS, &mut s.system_buttons, delta, wrap)
        }
        RowId::GuideGesture => {
            if !s.gamepad_forwarding {
                return false;
            }
            step_str(&GUIDE_GESTURE, &mut s.guide_gesture, delta, wrap)
        }
        RowId::PadHaptics => {
            if !s.gamepad_forwarding {
                return false;
            }
            toggle(&mut s.pad_haptics, delta, wrap)
        }
        RowId::PadSpeaker => {
            if !s.gamepad_forwarding {
                return false;
            }
            // On/Off over the stored string, the way the GTK switch edits the same key: a
            // stored "mix" reads as Off (it renders as off today) and any step writes the
            // two values that do something.
            let mut on = pad_speaker_on(&s.pad_speaker);
            toggle(&mut on, delta, wrap)
                .map(|()| s.pad_speaker = if on { "pad" } else { "off" }.to_string())
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
        RowId::Palette => {
            let all = &crate::library::PALETTES;
            let cur = all.iter().position(|p| p.id == s.ui_palette);
            step_option(cur, all.len(), delta, wrap).map(|i| s.ui_palette = all[i].id.to_string())
        }
        RowId::ReduceMotion => toggle(&mut s.reduce_motion, delta, wrap),
        RowId::ReduceUiResolution => {
            toggle_extra(s, android_keys::REDUCE_UI_RES, false, delta, wrap)
        }
        RowId::LibraryView => {
            let all = &crate::library::LibraryView::ALL;
            let cur = crate::library::LibraryView::parse(&s.library_view);
            let at = all.iter().position(|v| *v == cur);
            step_option(at, all.len(), delta, wrap).map(|i| s.library_view = all[i].id().into())
        }
        RowId::LibraryCollections => toggle(&mut s.library_collections, delta, wrap),
        RowId::Fullscreen => toggle(&mut s.fullscreen_on_stream, delta, wrap),
        RowId::AutoWake => toggle(&mut s.auto_wake, delta, wrap),
        RowId::LowLatency => toggle_extra(s, android_keys::LOW_LATENCY, true, delta, wrap),
        RowId::PhoneRumble => toggle_extra(s, android_keys::PHONE_RUMBLE, false, delta, wrap),
        RowId::PhoneGyro => toggle_extra(s, android_keys::PHONE_GYRO, false, delta, wrap),
        RowId::Sc2Passthrough => toggle_extra(s, android_keys::SC2, true, delta, wrap),
        RowId::DsCapture => toggle_extra(s, android_keys::DS_CAPTURE, true, delta, wrap),
        RowId::GamepadUi => toggle_extra(s, android_keys::GAMEPAD_UI, true, delta, wrap),
        RowId::GamepadUiMode => {
            let mut v = extra_str(s, android_keys::GAMEPAD_UI_MODE, "connected").to_string();
            step_str(&GAMEPAD_UI_MODES, &mut v, delta, wrap).map(|()| {
                s.extra.insert(
                    android_keys::GAMEPAD_UI_MODE.to_string(),
                    serde_json::Value::String(v),
                );
            })
        }
        // Navigation rows, handled before the settings path in `menu` — never a value edit.
        RowId::Profile(_) | RowId::NoProfiles | RowId::Controllers | RowId::Licenses => None,
    }
    .is_some()
}

/// The shared stepping rule: clamp when adjusting, wrap when cycling; an unknown
/// current value snaps to the first option on any step.
///
/// Reachable from the sibling screens because the library's own view/sort bar edits two of
/// the very values this screen's rows edit, and "◀ ▶ stops at the ends" is a grammar the
/// console states once. A second copy of it there would be a second place for the boundary
/// thud to go missing.
pub(super) fn step_option(
    current: Option<usize>,
    len: usize,
    delta: i32,
    wrap: bool,
) -> Option<usize> {
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
pub(super) mod tests {
    use super::*;
    use pf_client_core::trust::Settings;

    /// The section names, against the shared vectors. The comment above [`TABS`] claims a setting
    /// is found under the same word on every client; this is what makes that claim checkable.
    ///
    /// The desktop has one tab the mobile clients do not — Input, holding touch mode, mouse,
    /// invert-scroll and shortcuts, which are desktop-host settings with nothing to set on a phone
    /// or a TV. The vectors model it with a `desktop_only` flag rather than omitting it, because a
    /// flat six-name list would red this test on day one and a seven-name list would red both
    /// mobile clients: the disagreement is real and belongs in the contract, not in prose.
    #[test]
    fn tab_names_match_the_shared_vectors() {
        let raw = include_str!("../../../../clients/shared/console-vectors.json");
        let file: serde_json::Value =
            serde_json::from_str(raw).expect("console-vectors.json must parse");
        let want: Vec<&str> = file["tabs"]
            .as_array()
            .expect("tabs")
            .iter()
            .map(|t| t["name"].as_str().expect("tab name"))
            .collect();
        let got: Vec<&str> = TABS.iter().map(|(name, _)| *name).collect();
        assert_eq!(got, want, "the desktop console's tab names and order");
    }

    fn ctx_parts() -> (Settings, Vec<pf_client_core::menu_nav::PadInfo>) {
        (Settings::default(), Vec::new())
    }

    /// Point the settings store at a throwaway HOME. `apply_row` rebases on the FILE
    /// before a mutating press and saves after it, so a test driving that path against the
    /// real `$HOME` would rewrite the developer's own console settings.
    ///
    /// Shared with the library screen's tests, which drive the same `Settings::save` through
    /// the library bar: one `OnceLock` for the whole binary is what keeps the write to the
    /// environment sound, and a second copy of this would be exactly the race the SAFETY note
    /// below rules out.
    pub(crate) fn fake_home() {
        use std::sync::OnceLock;
        static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
        HOME.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("pf-settings-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            // SAFETY: runs at most once, inside `get_or_init` — concurrent `fake_home` callers
            // block until it returns, and nothing else in this binary mutates `HOME`. (The old
            // set after the closure ran on EVERY call, so two parallel tests could race the
            // write; setting once under the OnceLock is what makes this sound.)
            unsafe { std::env::set_var("HOME", &dir) };
            dir
        });
    }

    /// Render the screen once so its strip and list carry real geometry, then hand back a
    /// pointer aimed at the centre of `rect`. Hit-testing reads what was DRAWN, so a test
    /// that skipped the render would be testing nothing.
    fn rendered(screen: &mut SettingsScreen) -> f64 {
        let fonts = crate::theme::build_fonts().unwrap();
        let (w, h) = (1280i32, 800i32);
        let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        let k = f64::from(h) / 800.0;
        screen.render(
            surface.canvas(),
            Rect::from_ltrb(0.0, 64.0, w as f32, h as f32 - 86.0),
            k,
            1.0 / 60.0,
            &fonts,
            &mut ctx,
        );
        k
    }

    fn press(r: Rect) -> Pointer {
        Pointer {
            x: f64::from(r.center_x()),
            y: f64::from(r.center_y()),
            kind: crate::pointer::PointerKind::Press,
        }
    }

    fn with_ctx(f: impl FnOnce(&mut Ctx)) {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        f(&mut ctx);
    }

    /// The bug this all started from: the tabs answered the shoulder buttons and nothing
    /// else, so a mouse or a touchscreen could not change section at all.
    #[test]
    fn a_press_on_a_pill_selects_that_tab() {
        let mut s = SettingsScreen::with_profiles(Vec::new());
        rendered(&mut s);
        assert_eq!(s.tab, 0);
        for target in [3, 1, TABS.len() - 1, 0] {
            let pill = s.strip.pill(target).expect("the strip drew every pill");
            with_ctx(|ctx| {
                let mut fx = Outbox::default();
                assert!(s.pointer(press(pill), ctx, &mut fx), "the pill took it");
            });
            assert_eq!(s.tab, target, "pressing pill {target} selects it");
            // Selecting a tab re-lays the strip; re-render so the next pick is current.
            rendered(&mut s);
        }
    }

    /// …and each tab still keeps its own cursor when a POINTER is what switched it.
    #[test]
    fn a_pressed_tab_restores_that_tabs_cursor() {
        let mut s = SettingsScreen::with_profiles(Vec::new());
        rendered(&mut s);
        s.list.cursor = 2;
        let second = s.strip.pill(1).unwrap();
        with_ctx(|ctx| {
            let mut fx = Outbox::default();
            s.pointer(press(second), ctx, &mut fx);
        });
        assert_eq!(s.list.cursor, 0, "a fresh tab starts at its own top");
        rendered(&mut s);
        let first = s.strip.pill(0).unwrap();
        with_ctx(|ctx| {
            let mut fx = Outbox::default();
            s.pointer(press(first), ctx, &mut fx);
        });
        assert_eq!(s.list.cursor, 2, "coming back lands where it was left");
    }

    /// A press on a row focuses AND activates it — one click changes the value, the way a
    /// row that is its own control should behave.
    #[test]
    fn a_press_on_a_row_focuses_and_cycles_it() {
        fake_home();
        let mut s = SettingsScreen::with_profiles(Vec::new());
        rendered(&mut s);
        let first = s.list.row_rect(0).expect("the list drew its rows");
        let (mut settings, pads) = ctx_parts();
        settings.save(); // seat the fake HOME's file — `apply_row` rebases on it
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        // Row 0 of the leading tab is Resolution, whose first step is Native → Match
        // window: one field, one unambiguous effect to assert on.
        assert_eq!(s.row_ids(&ctx)[0], RowId::Resolution);
        let mut fx = Outbox::default();
        assert!(!ctx.settings.match_window);
        assert!(s.pointer(press(first), &mut ctx, &mut fx));
        assert_eq!(s.list.cursor, 0, "the pressed row takes focus");
        assert!(
            ctx.settings.match_window,
            "one press both focuses the row and cycles its value"
        );
    }

    /// A press that lands on neither a pill nor a row is refused, so the shell can let it
    /// fall through rather than swallowing every stray click.
    #[test]
    fn a_press_on_empty_space_is_not_consumed() {
        let mut s = SettingsScreen::with_profiles(Vec::new());
        rendered(&mut s);
        with_ctx(|ctx| {
            let mut fx = Outbox::default();
            let p = Pointer {
                x: 4.0,
                y: 780.0,
                kind: crate::pointer::PointerKind::Press,
            };
            assert!(!s.pointer(p, ctx, &mut fx));
        });
    }

    /// The controller-audio rows: they follow the forwarding switch like every other pad
    /// row, and the speaker row edits the stored STRING exactly the way the GTK switch
    /// over the same key does — a stored "mix" (the declared TODO that renders as off)
    /// reads as Off, and any step writes only the two values that do something.
    #[test]
    fn controller_audio_rows_follow_forwarding_and_speak_the_gtk_dialect() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        // Defaults: haptics on, speaker on the pad.
        assert!(ctx.settings.pad_haptics);
        assert_eq!(ctx.settings.pad_speaker, "pad");
        assert!(adjust(RowId::PadHaptics, 1, true, &mut ctx));
        assert!(!ctx.settings.pad_haptics);
        assert!(adjust(RowId::PadSpeaker, 1, true, &mut ctx));
        assert_eq!(ctx.settings.pad_speaker, "off");
        assert!(adjust(RowId::PadSpeaker, 1, true, &mut ctx));
        assert_eq!(ctx.settings.pad_speaker, "pad");
        // A stored "mix" reads as Off and steps onto a value that works.
        ctx.settings.pad_speaker = "mix".into();
        assert!(adjust(RowId::PadSpeaker, 1, true, &mut ctx));
        assert_eq!(ctx.settings.pad_speaker, "pad");
        // Forwarding off parks both, like the sibling pad rows.
        ctx.settings.gamepad_forwarding = false;
        assert!(!adjust(RowId::PadHaptics, 1, true, &mut ctx));
        assert!(!adjust(RowId::PadSpeaker, 1, true, &mut ctx));
    }

    #[test]
    fn adjust_clamps_and_activate_wraps() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
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
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
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
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        assert!(!row_spec(RowId::EchoCancel, &ctx, &[]).enabled);
        assert!(
            !adjust(RowId::EchoCancel, -1, false, &mut ctx),
            "mic off = thud"
        );
        assert!(!adjust(RowId::EchoCancel, 1, true, &mut ctx), "A too");
        assert!(ctx.settings.echo_cancel, "and nothing was written");

        ctx.settings.mic_enabled = true;
        assert!(row_spec(RowId::EchoCancel, &ctx, &[]).enabled);
        assert!(adjust(RowId::EchoCancel, -1, false, &mut ctx));
        assert!(!ctx.settings.echo_cancel);
        assert!(adjust(RowId::EchoCancel, 1, true, &mut ctx));
        assert!(ctx.settings.echo_cancel);
    }

    /// The smoothness buffer is OFFERED only under Smoothness — under Lowest latency it names
    /// a quantity that doesn't exist, so the row is gone from the Video tab rather than sitting
    /// there dimmed. This is what the GTK and WinUI shells and the Apple/Android screens have
    /// always done; this screen was the exception until its row list stopped being fixed.
    #[test]
    fn smoothness_buffer_is_offered_only_under_smoothness() {
        let (mut settings, pads) = ctx_parts();
        assert_eq!(settings.present_priority, "latency", "the shipped default");
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        let mut s = SettingsScreen::with_profiles(Vec::new());
        s.tab = TABS
            .iter()
            .position(|(name, _)| *name == "Video")
            .expect("the Video tab");

        let video = s.row_ids(&ctx);
        assert!(
            !video.contains(&RowId::SmoothBuffer),
            "latency hides the buffer row: {video:?}"
        );
        assert!(video.contains(&RowId::PresentPriority), "the intent stays");
        // Even reached out of band it writes nothing — the list it came from is a frame old.
        assert!(
            !adjust(RowId::SmoothBuffer, 1, false, &mut ctx),
            "latency intent = thud"
        );
        assert_eq!(ctx.settings.smooth_buffer, 0, "and nothing was written");

        // Stepping the intent to Smoothness brings the row into the list, directly under it.
        assert!(adjust(RowId::PresentPriority, 1, false, &mut ctx));
        assert_eq!(ctx.settings.present_priority, "smooth");
        let video = s.row_ids(&ctx);
        let intent = video
            .iter()
            .position(|id| *id == RowId::PresentPriority)
            .expect("the intent row");
        assert_eq!(
            video.get(intent + 1),
            Some(&RowId::SmoothBuffer),
            "the row that comes and goes sits BELOW the row that decides it, so the cursor \
             never has anything move out from under it"
        );
        assert!(adjust(RowId::SmoothBuffer, 1, false, &mut ctx));
        assert_eq!(ctx.settings.smooth_buffer, 1);

        // The intent wraps back and the row leaves again — with the cursor parked on the
        // intent row, which is where a user who just stepped it necessarily is.
        s.list.cursor = intent;
        assert!(adjust(RowId::PresentPriority, -1, false, &mut ctx));
        assert_eq!(ctx.settings.present_priority, "latency");
        let video = s.row_ids(&ctx);
        assert!(!video.contains(&RowId::SmoothBuffer));
        assert_eq!(
            video.get(s.list.cursor),
            Some(&RowId::PresentPriority),
            "the cursor is still on the row the user was stepping"
        );
    }

    /// A cursor parked past the end of a list that shrank underneath it is pulled back rather
    /// than indexed with — the console must not panic because another writer changed the
    /// presentation intent while its settings screen was open.
    #[test]
    fn a_shrinking_list_pulls_the_cursor_back() {
        // `apply_row` rebases on the FILE before acting, so this has to be seated — and
        // seated with the SHRUNKEN list's intent, which is the state being tested.
        fake_home();
        let (mut settings, pads) = ctx_parts();
        settings.present_priority = "latency".into();
        settings.save();
        settings.present_priority = "smooth".into();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        let mut s = SettingsScreen::with_profiles(Vec::new());
        s.tab = TABS
            .iter()
            .position(|(name, _)| *name == "Video")
            .expect("the Video tab");
        // Park on the last row while the buffer row is still there…
        s.list.cursor = s.row_ids(&ctx).len() - 1;
        let parked = s.list.cursor;
        // …then take it away behind the screen's back, as a desktop shell would.
        ctx.settings.present_priority = "latency".into();
        let mut fx = Outbox::default();
        let pulse = s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(pulse.is_some(), "the press was routed, not dropped");
        assert!(s.list.cursor < parked, "the cursor came back onto the list");
        assert!(fx.nav.is_none());
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
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
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
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
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
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        assert!(adjust(RowId::Bitrate, 1, false, &mut ctx));
        assert_eq!(ctx.settings.bitrate_kbps, 0, "snapped to Automatic");
    }

    /// The Profiles section trails the settings rows: one row per catalog profile whose
    /// value counts the pinned cards in the live model, activating opens the pin screen,
    /// and left/right (which edits every other row) is a boundary — a profile row
    /// navigates, it must never fall into the settings save path.
    #[test]
    fn profile_rows_navigate_instead_of_editing() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut pinned = crate::model::HostRow {
            key: "aa\0p1".into(),
            name: "Tower".into(),
            addr: "10.0.0.9".into(),
            port: 9777,
            fp_hex: "aa".into(),
            paired: true,
            saved: true,
            online: true,
            mgmt_port: 47990,
            can_wake: false,
            clipboard_sync: false,
            last_used: None,
            os: String::new(),
            pin: Some(crate::model::ProfileChip {
                id: "p1".into(),
                name: "Work".into(),
                accent: None,
            }),
            bound_profile: None,
        };
        let hosts = [pinned.clone(), {
            pinned.key = "aa".into();
            pinned.pin = None;
            pinned
        }];
        let mut ctx = Ctx {
            hosts: &hosts,
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        let mut s = SettingsScreen::with_profiles(vec![
            ("p1".into(), "Work".into()),
            ("p2".into(), "Game".into()),
        ]);
        s.tab = PROFILES_TAB;
        let ids = s.row_ids(&ctx);
        assert_eq!(ids, vec![RowId::Profile(0), RowId::Profile(1)]);

        let spec = row_spec(RowId::Profile(0), &ctx, &s.profiles);
        assert_eq!(spec.header, None, "the tab pill names the section");
        assert_eq!(spec.label, "Work");
        assert_eq!(spec.value.as_deref(), Some("Pinned to 1 host"));
        let spec = row_spec(RowId::Profile(1), &ctx, &s.profiles);
        assert_eq!(spec.value.as_deref(), Some("Not pinned"));

        s.list.cursor = 0; // onto "Work"
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(
            matches!(fx.nav, Some(crate::screens::Nav::Push(b))
                if matches!(*b, Screen::PinHosts(ref p) if p.profile_name() == "Work")),
            "A on a profile row opens its pin screen"
        );

        let mut fx = Outbox::default();
        let pulse = s.menu(
            MenuEvent::Move(pf_client_core::menu_nav::MenuDir::Right),
            &mut ctx,
            &mut fx,
        );
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
        assert!(fx.nav.is_none() && fx.cmds.is_empty());
    }

    /// An empty catalog shows the explainer placeholder — present, inert, and dimmed —
    /// so the section still tells the user where profiles come from.
    #[test]
    fn empty_catalog_shows_the_placeholder() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        let mut s = SettingsScreen::with_profiles(Vec::new());
        s.tab = PROFILES_TAB;
        let ids = s.row_ids(&ctx);
        assert_eq!(ids, vec![RowId::NoProfiles]);
        let spec = row_spec(RowId::NoProfiles, &ctx, &s.profiles);
        assert!(!spec.enabled);

        s.list.cursor = ids.len() - 1;
        let mut fx = Outbox::default();
        let pulse = s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
        assert!(fx.nav.is_none());
    }

    /// Every row the screen knows about must live in exactly one tab — a row missing from
    /// [`TABS`] is a setting that became unreachable in Gaming Mode, which is precisely
    /// what this screen exists to prevent.
    /// The desktop row set is untouched by the platform split (the non-regression contract),
    /// and Android drops exactly the desktop-only concepts — nothing else.
    #[test]
    fn platform_row_split_hides_only_the_other_platforms_concepts() {
        use crate::platform::Platform;
        let all: Vec<RowId> = TABS
            .iter()
            .flat_map(|(_, rows)| rows.iter().copied())
            .collect();
        // The desktop shows exactly what it showed before there was a second platform: the
        // Android rows are the only ones missing there.
        let off_desktop: Vec<RowId> = all
            .iter()
            .copied()
            .filter(|id| !row_on(*id, Platform::Desktop))
            .collect();
        assert_eq!(
            off_desktop,
            vec![
                RowId::LowLatency,
                RowId::PhoneRumble,
                RowId::PhoneGyro,
                RowId::Sc2Passthrough,
                RowId::DsCapture,
                RowId::Controllers,
                // Between the Input tab's rows and the rest of Interface: this one sits under
                // Reduce motion, which is earlier in that tab than the console-UI switch.
                RowId::ReduceUiResolution,
                RowId::GamepadUi,
                RowId::GamepadUiMode,
                RowId::Licenses,
            ]
        );
        let off_android: Vec<RowId> = all
            .iter()
            .copied()
            .filter(|id| !row_on(*id, Platform::Android))
            .collect();
        assert_eq!(
            off_android,
            vec![
                RowId::Decoder,
                RowId::Chroma444,
                RowId::Vsync,
                RowId::AllowVrr,
                RowId::Shortcuts,
                RowId::Fullscreen,
            ]
        );
        // Every row belongs to at least one platform.
        assert!(all
            .iter()
            .all(|id| row_on(*id, Platform::Desktop) || row_on(*id, Platform::Android)));
    }

    /// The Android rows edit `Settings::extra` under their `android.*` keys and nothing
    /// else — a toggle there must not touch a typed field, and a fresh store reads each row's
    /// documented default.
    #[test]
    fn android_rows_live_in_extra() {
        with_ctx(|ctx| {
            ctx.platform = crate::platform::Platform::Android;
            let before = ctx.settings.clone();
            assert!(extra_bool(ctx.settings, android_keys::LOW_LATENCY, true));
            assert!(adjust(RowId::LowLatency, 1, true, ctx));
            assert!(!extra_bool(ctx.settings, android_keys::LOW_LATENCY, true));
            assert!(adjust(RowId::GamepadUiMode, 1, true, ctx));
            assert_eq!(
                extra_str(ctx.settings, android_keys::GAMEPAD_UI_MODE, "connected"),
                "always"
            );
            assert!(extra_bool(ctx.settings, android_keys::GAMEPAD_UI, true));
            assert!(adjust(RowId::GamepadUi, 1, true, ctx));
            assert!(!extra_bool(ctx.settings, android_keys::GAMEPAD_UI, true));
            // Only `extra` moved.
            let mut after = ctx.settings.clone();
            after.extra = before.extra.clone();
            assert_eq!(after, before);
        });
    }

    /// The console-off switch exists only where there is a fallback interface for "off"
    /// to land in, and the mode row under it only where the mode decides anything: not on
    /// a TV (always console, whatever the mode says) and not while the switch is off.
    #[test]
    fn console_off_switch_needs_a_fallback_ui() {
        with_ctx(|ctx| {
            ctx.platform = crate::platform::Platform::Android;
            // A TV: no off switch (it would strand the user), and no mode row either —
            // `gamepadUiActive`'s tv term satisfies the OR on its own.
            assert!(
                !row_applies(RowId::GamepadUi, ctx),
                "a TV offers no off switch"
            );
            assert!(!row_applies(RowId::GamepadUiMode, ctx));
            // A phone or tablet with the console on: both rows.
            ctx.fallback_ui = true;
            assert!(row_applies(RowId::GamepadUi, ctx));
            assert!(row_applies(RowId::GamepadUiMode, ctx));
            // Switched off: the switch stays (it is the way back), the mode row goes.
            set_extra_bool(ctx.settings, android_keys::GAMEPAD_UI, false);
            assert!(row_applies(RowId::GamepadUi, ctx));
            assert!(
                !row_applies(RowId::GamepadUiMode, ctx),
                "the mode row decides nothing while the switch above it is off"
            );
        });
    }

    #[test]
    fn every_row_has_exactly_one_tab() {
        let mut seen: Vec<RowId> = Vec::new();
        for (_, rows) in &TABS {
            for id in *rows {
                assert!(!seen.contains(id), "{id:?} is in two tabs");
                seen.push(*id);
            }
        }
        // The pre-tab flat list, plus the palette row, the lossless-audio row, the
        // reduce-motion row and the two controller-audio rows (haptics + speaker — the
        // 2026-08 sweep found them bridged but unreachable) later passes added, minus the
        // game-library toggle: this screen never read it, and the library is offered on any
        // paired host now.
        // 35 desktop rows + the ten Android-only ones (design android-skia-console-port.md
        // D3): eight `extra`-backed settings and two platform-screen action rows.
        assert_eq!(seen.len(), 45, "{seen:?}");
        assert!(seen.contains(&RowId::Palette));
        assert!(seen.contains(&RowId::ReduceMotion));
        assert!(seen.contains(&RowId::ReduceUiResolution));
        assert!(seen.contains(&RowId::AudioFormat));
        // The catalog rows belong to the trailing tab, which builds them at render time.
        assert!(TABS[PROFILES_TAB].1.is_empty());
        assert_eq!(TABS[PROFILES_TAB].0, "Profiles");
    }

    /// The collections entry is a plain off-by-default toggle, and it sits directly under the
    /// row that says what the library looks like — the two are one decision read in two
    /// halves, and a user who wants the tiles goes looking beside the arrangement.
    ///
    /// Off by default matters more than the placement: this key decides where a deep link
    /// lands, so an install that never opens this screen must keep the shelf it has.
    #[test]
    fn the_collections_entry_sits_with_the_library_view_and_ships_off() {
        let (mut settings, pads) = ctx_parts();
        assert!(!settings.library_collections, "off by default");
        let interface = TABS
            .iter()
            .find(|(name, _)| *name == "Interface")
            .expect("the Interface tab")
            .1;
        let view = interface
            .iter()
            .position(|id| *id == RowId::LibraryView)
            .expect("the library view row");
        assert_eq!(interface.get(view + 1), Some(&RowId::LibraryCollections));

        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        assert!(
            !adjust(RowId::LibraryCollections, -1, false, &mut ctx),
            "already off = thud"
        );
        assert!(adjust(RowId::LibraryCollections, 1, false, &mut ctx));
        assert!(ctx.settings.library_collections);
        assert_eq!(
            row_spec(RowId::LibraryCollections, &ctx, &[])
                .value
                .as_deref(),
            Some("On"),
            "the row says what the key holds"
        );
        assert!(
            !adjust(RowId::LibraryCollections, 1, false, &mut ctx),
            "on = thud"
        );
        assert!(
            adjust(RowId::LibraryCollections, 1, true, &mut ctx),
            "A flips it back"
        );
        assert!(!ctx.settings.library_collections);
    }

    /// L1/R1 wrap around the strip and each tab keeps its own cursor, so a detour into
    /// another section doesn't lose your place.
    #[test]
    fn shoulders_cycle_tabs_and_keep_each_cursor() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        let mut s = SettingsScreen::with_profiles(Vec::new());
        let mut fx = Outbox::default();
        assert_eq!(s.tab, 0);
        s.list.cursor = 3; // "Bitrate", in Stream
        s.menu(MenuEvent::JumpForward, &mut ctx, &mut fx);
        assert_eq!(s.tab, 1);
        assert_eq!(s.list.cursor, 0, "a fresh tab starts at its first row");
        s.list.cursor = 2; // "10-bit HDR", in Video
        s.menu(MenuEvent::JumpBack, &mut ctx, &mut fx);
        assert_eq!((s.tab, s.list.cursor), (0, 3), "Stream kept its place");
        // Backwards off the first tab wraps to the last…
        s.menu(MenuEvent::JumpBack, &mut ctx, &mut fx);
        assert_eq!(s.tab, PROFILES_TAB);
        // …whose (catalog-built) length clamps a remembered cursor that no longer fits.
        assert_eq!(s.list.cursor, 0);
        s.menu(MenuEvent::JumpForward, &mut ctx, &mut fx);
        assert_eq!(s.tab, 0);
        // Switching sections is navigation, never a settings write.
        assert!(fx.nav.is_none() && fx.cmds.is_empty());
    }

    /// The lossless opt-in: it ships OFF, steps the cross-client table verbatim, sits directly
    /// under the channel count, and follows it — dim and inert under 5.1/7.1, because this
    /// client's session refuses to ASK for lossless surround (see the `enabled` note in
    /// [`row_spec`]; the host's own decline is gone). Every value is asserted against
    /// `pf_client_core::audio_format`'s constants rather than restated, so a spelling change there
    /// reds this test instead of quietly making the console write a key nobody reads.
    #[test]
    fn audio_format_ships_off_and_follows_the_channel_count() {
        use pf_client_core::audio_format::{AUDIO_FORMAT_LOSSLESS_48, AUDIO_FORMAT_LOSSLESS_96};
        let (mut settings, pads) = ctx_parts();
        assert_eq!(settings.audio_format, AUDIO_FORMAT_OPUS, "off by default");
        assert_eq!(settings.audio_channels, 2, "…and the gate starts open");
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        let mut s = SettingsScreen::with_profiles(Vec::new());
        s.tab = TABS
            .iter()
            .position(|(name, _)| *name == "Audio")
            .expect("the Audio tab");
        let audio = s.row_ids(&ctx);
        let channels = audio
            .iter()
            .position(|id| *id == RowId::Audio)
            .expect("the channels row");
        assert_eq!(
            audio.get(channels + 1),
            Some(&RowId::AudioFormat),
            "the row sits directly under the one that dims it, like every other pair here"
        );

        // Steps the shared table in order, clamping at both ends…
        assert!(
            !adjust(RowId::AudioFormat, -1, false, &mut ctx),
            "already Opus = thud"
        );
        assert!(adjust(RowId::AudioFormat, 1, false, &mut ctx));
        assert_eq!(ctx.settings.audio_format, AUDIO_FORMAT_LOSSLESS_48);
        assert!(adjust(RowId::AudioFormat, 1, false, &mut ctx));
        assert_eq!(ctx.settings.audio_format, AUDIO_FORMAT_LOSSLESS_96);
        assert!(
            !adjust(RowId::AudioFormat, 1, false, &mut ctx),
            "last = thud"
        );
        // …and A wraps home, so every rung is reachable one-handed.
        assert!(adjust(RowId::AudioFormat, 1, true, &mut ctx));
        assert_eq!(ctx.settings.audio_format, AUDIO_FORMAT_OPUS);

        // Surround dims it AND refuses the write. A row that accepted a change the session
        // throws away is the same lie as an enabled-looking control.
        ctx.settings.audio_format = AUDIO_FORMAT_LOSSLESS_48.into();
        ctx.settings.audio_channels = 6;
        assert!(!row_spec(RowId::AudioFormat, &ctx, &[]).enabled);
        assert!(
            !adjust(RowId::AudioFormat, 1, false, &mut ctx),
            "surround = thud"
        );
        assert!(!adjust(RowId::AudioFormat, 1, true, &mut ctx), "A too");
        assert_eq!(
            ctx.settings.audio_format, AUDIO_FORMAT_LOSSLESS_48,
            "and nothing was written — the stored preference survives the gate"
        );
        // Dimmed, never dropped: it stays visible beside the row that dimmed it.
        assert!(s.row_ids(&ctx).contains(&RowId::AudioFormat));
        ctx.settings.audio_channels = 2;
        assert!(row_spec(RowId::AudioFormat, &ctx, &[]).enabled);

        // A rung this build has no row for — a newer client's, arriving through a shared profile
        // catalog — reads as the Opus the session will actually run, not as a blank "—".
        ctx.settings.audio_format = AUDIO_FORMAT_OPUS.into();
        let opus = row_spec(RowId::AudioFormat, &ctx, &[]).value;
        assert!(opus.is_some());
        ctx.settings.audio_format = "lossless192".into();
        assert_eq!(row_spec(RowId::AudioFormat, &ctx, &[]).value, opus);
    }

    /// The palette row steps the shared `ui_palette` key through the table and wraps on A,
    /// like every other choice row.
    #[test]
    fn palette_row_steps_the_shared_key() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        assert_eq!(ctx.settings.ui_palette, "violet", "the brand default ships");
        assert_eq!(
            row_spec(RowId::Palette, &ctx, &[]).value.as_deref(),
            Some("Violet")
        );
        assert!(
            !adjust(RowId::Palette, -1, false, &mut ctx),
            "already the first = thud"
        );
        assert!(adjust(RowId::Palette, 1, false, &mut ctx));
        assert_eq!(ctx.settings.ui_palette, crate::library::PALETTES[1].id);
        // A from the last entry wraps home.
        ctx.settings.ui_palette = crate::library::PALETTES
            .last()
            .expect("non-empty")
            .id
            .to_string();
        assert!(adjust(RowId::Palette, 1, true, &mut ctx));
        assert_eq!(ctx.settings.ui_palette, "violet");
        // A store written by a newer client shows that client's value, not a blank row.
        ctx.settings.ui_palette = "chartreuse".into();
        assert_eq!(
            row_spec(RowId::Palette, &ctx, &[]).value.as_deref(),
            Some("Violet"),
            "an unknown palette reads as the default it actually draws"
        );
    }
}
