//! Console settings: the couch-facing subset of the shared Settings store.
//!
//! One row per setting, grouped by [`TABS`]. Left/right steps the focused value
//! (clamped); A cycles wrapping; L1/R1 change section; B closes. Every change
//! writes the store immediately so desktop shells round-trip the same file.
//! Each tab remembers its cursor. Profiles is built from the catalog at render
//! time — the console never creates or edits profiles.
//!
//! Tab names match `clients/shared/console-vectors.json`. Platform split:
//! [`row_on`]. Availability this frame: [`row_applies`].

use crate::glyphs::{Hint, HintKey};
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox, Screen};
use crate::theme::{fg, Fonts, W};
use crate::widgets::{
    permits, Charset, KeyMsg, Keyboard, ListMsg, MenuList, RowSpec, TabStrip, TAB_STRIP_H,
};
use pf_client_core::audio_format::{AUDIO_FORMATS, AUDIO_FORMAT_OPUS};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use pf_client_core::trust::{MouseMode, StatsVerbosity, TouchMode};
use skia_safe::{Canvas, Rect};

/// Dispatch key for adjust/activate. The pad list under "Use controller" can
/// churn between frames, so an index would act on the wrong row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowId {
    /// Index into [`SettingsScreen::profiles`]. Activate opens pin-to-hosts;
    /// the console never edits a profile.
    Profile(usize),
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
    /// `VIDEO_CAP_10BIT` without HDR. Desktop-only: Android takes depth from the panel.
    TenBitSdr,
    PresentPriority,
    SmoothBuffer,
    Vsync,
    AllowVrr,
    Audio,
    /// Cross-client `audio_format` key. Gated on stereo — see [`row_spec`].
    AudioFormat,
    /// `CLIENT_CAP_KEEP_HOST_AUDIO`. Advertised on every platform, including Android.
    KeepHostAudio,
    Mic,
    EchoCancel,
    PadForward,
    Pad,
    PadType,
    SystemButtons,
    GuideGesture,
    /// `trust::Settings::pad_haptics`. Negotiated: needs a capable host and a wired DS5.
    PadHaptics,
    /// `trust::Settings::pad_speaker`. On (`"pad"`) / Off only — stored `"mix"`
    /// renders as off, so offering it would be a no-op control.
    PadSpeaker,
    Touch,
    Mouse,
    InvertScroll,
    Shortcuts,
    /// `trust::Settings::overlay_actions`. Action row: opens [`super::ring_editor::RingEditorScreen`].
    QuickActions,
    Stats,
    Fullscreen,
    AutoWake,
    /// `trust::Settings::follow_os_theme`. Shown only while the embedder publishes
    /// a theme; [`RowId::Palette`] hides while it is on.
    FollowOsTheme,
    Palette,
    ReduceMotion,
    /// Android-only. Draw the console at 1080p; a 4K panel otherwise pays 4× fill.
    ReduceUiResolution,
    /// Same `library_view` key the library bar writes.
    LibraryView,
    /// `trust::Settings::library_collections`. Couch path besides the shelf's Y.
    LibraryCollections,
    // Android-only. Values live in `trust::Settings::extra` under `android.*`
    // so the typed struct stays shared; [`row_on`] keeps them off desktop.
    /// Slice-progressive decode plus DSCP. Android-only.
    LowLatency,
    PhoneRumble,
    PhoneGyro,
    /// Raw BLE/USB capture instead of the OS pad.
    Sc2Passthrough,
    /// Raw USB: touchpad, motion, adaptive triggers.
    DsCapture,
    /// `Settings.gamepadUiEnabled`. Only with [`Ctx::fallback_ui`] — otherwise
    /// off strands the user with no UI.
    GamepadUi,
    GamepadUiMode,
    /// Action row: opens the in-process controllers screen.
    Controllers,
    /// Action row: asks the host to open the platform licences screen.
    Licenses,
}

/// `Settings::extra` keys the Android rows share with Kotlin (`ConsoleJson.settings`).
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

/// Stored `android.gamepad_ui_mode` values (`GamepadUi.kt`).
const GAMEPAD_UI_MODES: [(&str, &str); 2] =
    [("connected", "With a controller"), ("always", "Always")];

/// `"pad"` is the only live value; `"mix"` renders as off. Local copy because
/// `pad_audio` is `cfg(linux|windows)` and Android still sends the setting.
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

// Tab names match Apple/Android (`console-vectors.json`). Profiles is empty here:
// its rows come from the catalog. Device pickers stay on the desktop dialogs.
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
            RowId::TenBitSdr,
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
            RowId::KeepHostAudio,
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
            RowId::QuickActions,
        ],
    ),
    (
        "Interface",
        &[
            RowId::FollowOsTheme,
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

/// Trailing Profiles tab — catalog-built, not [`TABS`] rows.
const PROFILES_TAB: usize = TABS.len() - 1;

/// Strip length for the shell's raster walk. `cfg(test)`: a shipping build
/// would warn it dead, and this crate treats warnings as errors.
#[cfg(test)]
pub(crate) const TAB_COUNT: usize = TABS.len();

const RESOLUTIONS: [(u32, u32); 6] = [
    (0, 0), // native: host follows the panel
    (1280, 720),
    (1280, 800), // Steam Deck panel
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];
const REFRESH: [u32; 5] = [0, 30, 60, 90, 120];
/// Must stay in sync with [`punktfunk_core::render_scale::PRESETS`].
const RENDER_SCALES: [f64; 9] = [0.5, 0.67, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0];
/// Left/right rungs in kbps. Denser below ~20 Mbps; ceiling 2 Gbps. Off-ladder
/// values go through the Y field rather than a longer ladder.
const BITRATES: [u32; 30] = [
    0, 1_000, 2_000, 3_000, 4_000, 5_000, 6_000, 8_000, 10_000, 12_000, 15_000, 20_000, 25_000,
    30_000, 40_000, 50_000, 60_000, 80_000, 100_000, 125_000, 150_000, 200_000, 250_000, 300_000,
    400_000, 500_000, 750_000, 1_000_000, 1_500_000, 2_000_000,
];
/// Typed-field ceiling in Mbps — the ladder's top. Host range is 500 kbps–8 Gbps.
const CUSTOM_MAX_MBPS: u32 = 2_000;
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
    // 100–400 Mbps class, 8-bit SDR. Host must support it; else HEVC.
    ("pyrowave", "PyroWave (wired LAN)"),
];
// Per-OS hardware rungs. Windows has no VAAPI (`Decoder::new` has no branch).
// Stored values are `native-*`; `migrate_decoder_pref` rewrites a legacy store
// on read, but until the user re-picks it will not match a preset here.
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
/// Shared `present_priority` key — one profile reads the same on every client.
const PRESENT_PRIORITIES: [(&str, &str); 2] =
    [("latency", "Lowest latency"), ("smooth", "Smoothness")];
/// Depth in frames. `0` = Automatic, which resolves to 2.
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
/// Shared `system_buttons` key. Auto sends to the host except in Gaming Mode,
/// where Steam on this device would open a second overlay on the same press.
const SYSTEM_BUTTONS: [(&str, &str); 3] = [
    ("auto", "Automatic"),
    ("forward", "Send to host"),
    ("local", "This device"),
];
const GUIDE_GESTURE: [(&str, &str); 3] = [("auto", "Automatic"), ("on", "On"), ("off", "Off")];

pub(crate) struct SettingsScreen {
    list: MenuList,
    strip: TabStrip,
    tab: usize,
    /// Per-tab cursor so a detour does not reset the one you left.
    tab_cursors: [usize; TABS.len()],
    /// `(id, name)` loaded once. The console cannot create profiles, so this is
    /// stable for the screen's lifetime.
    profiles: Vec<(String, String)>,
    /// D-pad focus on the strip. TV remotes have no shoulders and no Tab key.
    strip_focus: bool,
    /// Typed Mbps while Y has the bitrate field open. Y, not A, so A still cycles.
    custom_bitrate: Option<String>,
    /// Tray keyboard. Unused on Deck: Steam's keyboard types (same as add-host).
    keyboard: Keyboard,
}

impl SettingsScreen {
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
            strip_focus: false,
            custom_bitrate: None,
            keyboard: Keyboard::new(),
        }
    }

    /// True while the typed field is open; the run loop keeps SDL text input started.
    pub(crate) fn editing(&self) -> bool {
        self.custom_bitrate.is_some()
    }

    /// SDL text. Digits only; four chars is 2000 Mbps, the ceiling.
    pub(crate) fn text_input(&mut self, text: &str) {
        for ch in text.chars() {
            self.type_char(ch);
        }
    }

    fn type_char(&mut self, ch: char) -> bool {
        let Some(buf) = self.custom_bitrate.as_mut() else {
            return false;
        };
        if !permits(Charset::Digits, ch) || buf.chars().count() >= 4 {
            return false;
        }
        buf.push(ch);
        true
    }

    fn backspace(&mut self) -> bool {
        self.custom_bitrate.as_mut().and_then(String::pop).is_some()
    }

    pub(crate) fn edit_key(&mut self, key: crate::input::Key, ctx: &mut Ctx) -> bool {
        use crate::input::Key as K;
        if self.custom_bitrate.is_none() {
            return false;
        }
        match key {
            K::Backspace => {
                self.backspace();
                true
            }
            K::Return | K::Escape => {
                self.commit_custom(ctx);
                true
            }
            _ => false,
        }
    }

    /// Close the field. Empty or `0` is an abandoned edit, not Automatic (the first rung).
    fn commit_custom(&mut self, ctx: &mut Ctx) {
        let Some(text) = self.custom_bitrate.take() else {
            return;
        };
        let Ok(mbps) = text.parse::<u32>() else {
            return;
        };
        if mbps == 0 {
            return;
        }
        // Rebase first: another writer may have stored the file while the keyboard was up.
        *ctx.settings = ctx.store.load();
        ctx.settings.bitrate_kbps = mbps.min(CUSTOM_MAX_MBPS) * 1000;
        ctx.store.save(ctx.settings);
    }

    fn custom_menu(&mut self, ev: MenuEvent, ctx: &mut Ctx) -> Option<MenuPulse> {
        if ctx.deck {
            // Steam types via `text_input`; the pad only commits.
            return match ev {
                MenuEvent::Back | MenuEvent::Confirm => {
                    self.commit_custom(ctx);
                    Some(MenuPulse::Confirm)
                }
                _ => None,
            };
        }
        let (msg, pulse) = self.keyboard.menu(ev);
        match msg {
            KeyMsg::Type(c) => {
                if self.type_char(c) {
                    Some(MenuPulse::Move)
                } else {
                    Some(MenuPulse::Boundary)
                }
            }
            KeyMsg::Backspace => {
                if self.backspace() {
                    Some(MenuPulse::Move)
                } else {
                    Some(MenuPulse::Boundary)
                }
            }
            KeyMsg::Done => {
                self.commit_custom(ctx);
                Some(MenuPulse::Confirm)
            }
            KeyMsg::None => pulse,
        }
    }

    /// Filtered by [`row_on`] / [`row_applies`]. Profiles comes from the catalog.
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

    /// Pull the cursor back. The smoothness buffer (and other writers) can shrink the list
    /// between frames.
    fn clamp_cursor(&mut self, len: usize) {
        if self.list.cursor >= len {
            self.list.jump_to(len.saturating_sub(1));
        }
    }

    #[cfg(test)]
    pub(crate) fn tab_for_test(&self) -> usize {
        self.tab
    }

    /// Last-drawn row rect — hit tests press real coordinates.
    #[cfg(test)]
    pub(crate) fn row_rect_for_test(&self, i: usize) -> Option<Rect> {
        self.list.row_rect(i)
    }

    fn switch_tab(&mut self, delta: i32, ctx: &Ctx) -> Option<MenuPulse> {
        let n = TABS.len() as i32;
        self.show_tab((self.tab as i32 + delta).rem_euclid(n) as usize, ctx)
    }

    /// Park the outgoing tab's cursor, then jump. Pointer pills name a tab outright.
    fn show_tab(&mut self, tab: usize, ctx: &Ctx) -> Option<MenuPulse> {
        if tab >= TABS.len() {
            return None;
        }
        self.tab_cursors[self.tab] = self.list.cursor;
        self.tab = tab;
        // Remembered cursor can outlive a shorter tab (Profiles catalog, smoothness buffer).
        let len = self.row_ids(ctx).len();
        self.list
            .jump_to(self.tab_cursors[self.tab].min(len.saturating_sub(1)));
        Some(MenuPulse::Move)
    }

    /// Strip first: pills sit above the list, so a press there is never a row.
    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        if self.custom_bitrate.is_some() && !ctx.deck {
            if !self.keyboard.covers(p) {
                if p.press() {
                    self.commit_custom(ctx);
                    return true;
                }
                return false;
            }
            let (msg, _) = self.keyboard.pointer(p);
            match msg {
                KeyMsg::Type(c) => {
                    self.type_char(c);
                }
                KeyMsg::Backspace => {
                    self.backspace();
                }
                KeyMsg::Done => self.commit_custom(ctx),
                KeyMsg::None => {}
            }
            return true;
        }
        if let Some(tab) = self.strip.pointer(p) {
            self.show_tab(tab, ctx);
            return true;
        }
        // A press on the rows takes D-pad focus back from the strip.
        if p.press() {
            self.strip_focus = false;
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
        if self.custom_bitrate.is_some() {
            return self.custom_menu(ev, ctx);
        }
        if self.strip_focus {
            // D-pad tab path for remotes with no shoulders and no Tab key.
            return match ev {
                MenuEvent::Back => {
                    fx.pop();
                    None
                }
                MenuEvent::Move(MenuDir::Left) | MenuEvent::JumpBack => self.switch_tab(-1, ctx),
                MenuEvent::Move(MenuDir::Right) | MenuEvent::JumpForward => self.switch_tab(1, ctx),
                MenuEvent::Move(MenuDir::Down) | MenuEvent::Confirm => {
                    self.strip_focus = false;
                    Some(MenuPulse::Move)
                }
                MenuEvent::Move(MenuDir::Up) => Some(MenuPulse::Boundary),
                _ => None,
            };
        }
        match ev {
            MenuEvent::Back => {
                fx.pop();
                return None;
            }
            MenuEvent::JumpBack => return self.switch_tab(-1, ctx),
            MenuEvent::JumpForward => return self.switch_tab(1, ctx),
            // Up from row 0 focuses the strip, not a boundary.
            MenuEvent::Move(MenuDir::Up) if self.list.cursor == 0 => {
                self.strip_focus = true;
                return Some(MenuPulse::Move);
            }
            _ => {}
        }
        let ids = self.row_ids(ctx);
        self.clamp_cursor(ids.len());
        // Y opens the typed bitrate. Skip under PyroWave: the row is inert (`row_spec`).
        if ev == MenuEvent::Secondary {
            return if ids.get(self.list.cursor) == Some(&RowId::Bitrate)
                && ctx.settings.codec != "pyrowave"
            {
                self.custom_bitrate = Some(String::new());
                Some(MenuPulse::Confirm)
            } else {
                None
            };
        }
        let (msg, pulse) = self.list.menu(ev, ids.len());
        self.apply_row(msg, pulse, &ids, ctx, fx)
    }

    /// Shared by pad and pointer so a click and an A press cannot drift apart.
    fn apply_row(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        ids: &[RowId],
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        // List shrank between clamp and here: drop the keypress, do not panic.
        let Some(&focused) = ids.get(self.list.cursor) else {
            return pulse;
        };
        // Profiles navigate; they must not hit the settings save path.
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
            // Action row: adjust is a boundary.
            RowId::QuickActions => {
                return match msg {
                    ListMsg::Activate => {
                        fx.push(Screen::RingEditor(Box::new(
                            super::ring_editor::RingEditorScreen::new(ctx),
                        )));
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            // In-process Skia screen; grant dialogs still go to the host.
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
            // Platform screen: A asks the host to open it; nothing here edits.
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
        // Whole-file writer: rebase before mutate or another writer's store is reverted.
        // Cursor moves must not touch the disk.
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
                if adjust(focused, 1, true, ctx) {
                    ctx.store.save(ctx.settings);
                }
                pulse
            }
            ListMsg::None => pulse,
        }
    }

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        if self.custom_bitrate.is_some() {
            if ctx.deck {
                return vec![
                    Hint::new(HintKey::Key("STEAM + X"), "Keyboard"),
                    Hint::new(HintKey::Confirm, "Done"),
                    Hint::new(HintKey::Back, "Done"),
                ];
            }
            return vec![
                Hint::new(HintKey::Confirm, "Type"),
                Hint::new(HintKey::Tertiary, "Delete"),
                Hint::new(HintKey::Back, "Done"),
            ];
        }
        // Strip-focused: hints describe the D-pad, not the rows.
        if self.strip_focus {
            return vec![
                Hint::new(HintKey::Adjust, "Section"),
                Hint::new(HintKey::Confirm, "Rows"),
                Hint::new(HintKey::Back, "Done"),
            ];
        }
        let ids = self.row_ids(ctx);
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
            // Inert under PyroWave (`row_spec`): no Adjust hint.
            Some(RowId::Bitrate) if ctx.settings.codec == "pyrowave" => {
                vec![Hint::new(HintKey::Back, "Done")]
            }
            Some(RowId::Bitrate) => vec![
                Hint::new(HintKey::Adjust, "Adjust"),
                Hint::new(HintKey::Secondary, "Type a rate"),
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
        // Strip on top, explainer under the list; rows get the band between.
        let detail_h = 34.0 * k;
        let strip_h = TAB_STRIP_H * k;
        let labels: Vec<&str> = TABS.iter().map(|(name, _)| *name).collect();
        self.strip.render(
            canvas,
            Rect::from_ltrb(rect.left, rect.top, rect.right, rect.top + strip_h as f32),
            &labels,
            self.tab,
            self.strip_focus,
            fonts,
            k,
            dt,
        );
        let seat = self
            .keyboard
            .seat(self.custom_bitrate.is_some() && !ctx.deck, dt);
        let tray_h = if seat > 0.0 {
            (Keyboard::tray_height() + 12.0) * k * seat
        } else {
            0.0
        };
        let list_rect = Rect::from_ltrb(
            rect.left,
            rect.top + strip_h as f32,
            rect.right,
            rect.bottom - detail_h as f32 - tray_h as f32,
        );
        let ids = self.row_ids(ctx);
        self.clamp_cursor(ids.len());
        let mut rows: Vec<RowSpec> = ids
            .iter()
            .map(|id| row_spec(*id, ctx, &self.profiles))
            .collect();
        // Field-open: the Bitrate row shows the typed digits and the caret.
        if let (Some(text), Some(i)) = (
            self.custom_bitrate.as_ref(),
            ids.iter().position(|id| *id == RowId::Bitrate),
        ) {
            rows[i].value = Some(if text.is_empty() {
                "Mbps".into()
            } else {
                format!("{text} Mbps")
            });
            rows[i].value_dim = text.is_empty();
            rows[i].caret = true;
        }
        self.list.render(
            canvas,
            list_rect,
            &rows,
            fonts,
            k,
            dt,
            // No row focus ring while the tray or the strip holds it.
            self.custom_bitrate.is_none() && !self.strip_focus,
        );
        let detail = ids
            .get(self.list.cursor)
            .copied()
            .map_or("", |id| detail(id, ctx));
        fonts.centered(
            canvas,
            detail,
            W::Regular,
            13.0 * k,
            fg(0.55),
            f64::from(rect.left) + f64::from(rect.width()) / 2.0,
            f64::from(rect.bottom) - detail_h - tray_h + 6.0 * k,
            f64::from(rect.width()) * 0.8,
        );
        if seat > 0.0 {
            self.keyboard.render(
                canvas,
                fonts,
                f64::from(rect.width()),
                f64::from(rect.bottom),
                seat,
                k,
            );
        }
    }
}

/// Whether this platform has the row at all.
///
/// [`TABS`] is the union so a setting sits under the same word on every client.
/// A concept the platform does not have is absent, never a no-op control.
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
            | RowId::TenBitSdr
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

/// Offered this frame, as opposed to offered-but-inert.
///
/// Echo cancel and pad rows dim under a parent switch so the relationship stays
/// visible. Smoothness buffer is a knob on one of two intents — under Lowest
/// latency the quantity does not exist, so the row is dropped. It sits directly
/// below the intent row so the cursor is never on a row that vanishes.
fn row_applies(id: RowId, ctx: &Ctx) -> bool {
    match id {
        RowId::SmoothBuffer => ctx.settings.present_priority == "smooth",
        // Needs `fallback_ui`; otherwise off strands the user with no UI.
        RowId::GamepadUi => ctx.fallback_ui,
        // Hidden unless fallback_ui and the switch above is on. Sits below that
        // switch so the cursor is never on a row that vanishes. A TV is always
        // console (`GamepadUi.kt`: the tv term alone satisfies the OR).
        RowId::GamepadUiMode => {
            ctx.fallback_ui && extra_bool(ctx.settings, android_keys::GAMEPAD_UI, true)
        }
        // `os_theme::available()`, not platform: a new publisher needs no edit here.
        RowId::FollowOsTheme => crate::os_theme::available(),
        // Hidden while follow_os_theme; sits below the switch that drops it.
        RowId::Palette => !(ctx.settings.follow_os_theme && crate::os_theme::available()),
        _ => true,
    }
}

fn row_spec(id: RowId, ctx: &Ctx, profiles: &[(String, String)]) -> RowSpec {
    // Pin count from live host rows, matching the carousel.
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
        RowId::QuickActions => {
            let mut r = RowSpec::action("Quick actions", true);
            r.header = Some("Quick actions");
            return r;
        }
        _ => {}
    }
    let s = &ctx.settings;
    // Dim under a parent switch (relationship stays visible). Smoothness buffer
    // is dropped instead — see [`row_applies`].
    let enabled = match id {
        RowId::EchoCancel => s.mic_enabled,
        // PyroWave ignores stored bitrate (session sends 0). Dim; keep the value.
        RowId::Bitrate => s.codec != "pyrowave",
        // Session still drops lossless unless `audio_channels == 2` (before the wire).
        // A live row under surround would change nothing. Delete this arm when that
        // filter learns the frame ladder — not before.
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
                bitrate_label(s.bitrate_kbps)
            },
        ),
        RowId::Compositor => (
            None,
            "Compositor",
            label_for(&COMPOSITORS, &s.compositor).into(),
        ),
        RowId::Codec => (None, "Video codec", label_for(&CODECS, &s.codec).into()),
        // Migrate before lookup or a legacy store (`vulkan`/`vaapi`) shows "—".
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
        RowId::TenBitSdr => (None, "10-bit SDR", on_off(s.ten_bit_sdr).into()),
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
        RowId::KeepHostAudio => (
            None,
            "Keep host audio playing",
            on_off(s.keep_host_audio).into(),
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
        RowId::FollowOsTheme => (
            None,
            "Follow system theme",
            on_off(s.follow_os_theme).into(),
        ),
        RowId::Palette => (
            None,
            "Background",
            crate::library::palette(&s.ui_palette).name.into(),
        ),
        // Label is the reduction, so On means it is in effect.
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
            // Not "Controller UI": that collides with the row above.
            "Show it",
            label_for(
                &GAMEPAD_UI_MODES,
                extra_str(s, android_keys::GAMEPAD_UI_MODE, "connected"),
            )
            .into(),
        ),
        RowId::Profile(_)
        | RowId::NoProfiles
        | RowId::Controllers
        | RowId::Licenses
        | RowId::QuickActions => {
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

/// One-line explainer. Platform so Android is not taught desktop-only chords.
fn detail(id: RowId, ctx: &Ctx) -> &'static str {
    use crate::platform::Platform;
    let platform = ctx.platform;
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
        RowId::Bitrate if ctx.settings.codec == "pyrowave" => {
            "PyroWave sets its own rate from the stream mode (all-intra) — a fixed bitrate \
             doesn't apply. Pick another codec to use this setting."
        }
        RowId::Bitrate => {
            "Automatic uses the host's default (20 Mbps). Y types an exact rate, up to 2 Gbps."
        }
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
        RowId::TenBitSdr => {
            "Smoother gradients without HDR — the picture is encoded at 10-bit \
             precision. Needs an NVIDIA host; HDR takes over when it engages."
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
        RowId::KeepHostAudio => {
            "The host's own speakers or headphones keep playing while you stream. \
             Both ends hear the same audio; needs a host on 0.32 or newer."
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
        RowId::QuickActions => {
            "The ring a two-finger twist or Select+A opens in a stream: what its six buttons \
             hold, and the shortcut chords they can send. Edited on the ring itself."
        }
        RowId::Shortcuts => {
            "Alt+Tab, Super and friends reach the host while input is captured. \
             Off, they act on this device instead."
        }
        RowId::FollowOsTheme => {
            "The console wears your desktop's theme — background, text and accent — and \
             follows a theme switch live. Off, the Background row below picks the look."
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

/// Mbps below 1 Gbps, Gbps above. Decimal only when rounding would collide
/// (12.5 Mbps, 1.5 Gbps). Off-ladder rates are real (typed field, desktop spinner).
fn bitrate_label(kbps: u32) -> String {
    let unit = |v: f64, suffix: &str| {
        if (v - v.round()).abs() < 0.05 {
            format!("{} {suffix}", v.round())
        } else {
            format!("{v:.1} {suffix}")
        }
    };
    let mbps = f64::from(kbps) / 1000.0;
    if kbps >= 1_000_000 {
        unit(mbps / 1000.0, "Gbps")
    } else {
        unit(mbps, "Mbps")
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

/// Label for a stored `audio_format`. Unknown → Opus (what the session runs), not
/// [`label_for`]'s "—": a shared catalog can carry a newer client's rung.
fn audio_format_label(value: &str) -> &'static str {
    AUDIO_FORMATS
        .iter()
        .find(|(v, _)| *v == value)
        .or_else(|| AUDIO_FORMATS.iter().find(|(v, _)| *v == AUDIO_FORMAT_OPUS))
        .map_or("", |(_, l)| *l)
}

/// Step (`wrap=false`, clamp; `None` = boundary) or cycle (`wrap=true`).
/// Toggles: left = off, right = on. A no-op is a boundary.
fn adjust(id: RowId, delta: i32, wrap: bool, ctx: &mut Ctx) -> bool {
    let s = &mut *ctx.settings;
    match id {
        RowId::Resolution => {
            // Native, Match window, then sizes. Match window is virtual index 1
            // (`match_window` flag, w/h cleared); RESOLUTIONS[0] is Native.
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
            // Writers store these literals; a hand-edited oddball snaps to the first step.
            let cur = RENDER_SCALES.iter().position(|v| *v == s.render_scale);
            step_option(cur, RENDER_SCALES.len(), delta, wrap)
                .map(|i| s.render_scale = RENDER_SCALES[i])
        }
        RowId::Bitrate => {
            // Inert under PyroWave (host pins the rate; see `row_spec`).
            if s.codec == "pyrowave" {
                return false;
            }
            // Off-ladder must not snap to Automatic (index 0). Step to the neighbour
            // the thumb is heading for.
            let stepped = match BITRATES.iter().position(|b| *b == s.bitrate_kbps) {
                Some(i) => step_option(Some(i), BITRATES.len(), delta, wrap),
                None if delta < 0 => BITRATES.iter().rposition(|b| *b < s.bitrate_kbps),
                // Above the ceiling: wrap goes to Automatic; clamp thuds.
                None => BITRATES
                    .iter()
                    .position(|b| *b > s.bitrate_kbps)
                    .or(if wrap { Some(0) } else { None }),
            };
            stepped.map(|i| s.bitrate_kbps = BITRATES[i])
        }
        RowId::Compositor => step_str(&COMPOSITORS, &mut s.compositor, delta, wrap),
        RowId::Codec => step_str(&CODECS, &mut s.codec, delta, wrap),
        RowId::Decoder => {
            // Migrate first or a legacy value jumps to first/last instead of its neighbour.
            s.decoder = pf_client_core::decoder_pref::migrate_decoder_pref(&s.decoder);
            step_str(&DECODERS, &mut s.decoder, delta, wrap)
        }
        RowId::Hdr => toggle(&mut s.hdr_enabled, delta, wrap),
        RowId::Chroma444 => toggle(&mut s.enable_444, delta, wrap),
        RowId::TenBitSdr => toggle(&mut s.ten_bit_sdr, delta, wrap),
        RowId::PresentPriority => {
            let cur = PRESENT_PRIORITIES
                .iter()
                .position(|(v, _)| *v == s.present_priority);
            step_option(cur, PRESENT_PRIORITIES.len(), delta, wrap)
                .map(|i| s.present_priority = PRESENT_PRIORITIES[i].0.to_string())
        }
        // Not offered under latency ([`row_applies`]). Reachable only if another
        // writer flipped intent between list-build and this keypress: thud, don't store.
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
        // Inert under surround (this client's request filter; see `row_spec`).
        RowId::AudioFormat => {
            if s.audio_channels == 2 {
                step_str(AUDIO_FORMATS, &mut s.audio_format, delta, wrap)
            } else {
                None
            }
        }
        RowId::KeepHostAudio => toggle(&mut s.keep_host_audio, delta, wrap),
        RowId::Mic => toggle(&mut s.mic_enabled, delta, wrap),
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
            // Automatic first, then connected pads by stable key.
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
            // `"mix"` reads Off; a step writes only `"pad"` / `"off"`.
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
        RowId::FollowOsTheme => toggle(&mut s.follow_os_theme, delta, wrap),
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
        // Navigation rows: handled in `apply_row` before the settings path.
        RowId::Profile(_)
        | RowId::NoProfiles
        | RowId::Controllers
        | RowId::Licenses
        | RowId::QuickActions => None,
    }
    .is_some()
}

/// Clamp when adjusting, wrap when cycling. Unknown current value snaps to first.
///
/// `pub(super)` so the library view/sort bar shares the boundary thud.
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

// `pub(crate)` so shell tests outside `screens` share `fake_home`.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use pf_client_core::trust::Settings;

    /// Tab names vs `console-vectors.json`. Input is `desktop_only` in the vectors:
    /// omitting it would fail this test; a seven-name list would fail the mobile clients.
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

    /// Throwaway config dir. `apply_row` rebases on the file, so a test against
    /// the real profile would rewrite the developer's settings.
    ///
    /// Redirects `HOME` on unix, `APPDATA` on Windows (`trust::config_dir`).
    /// One `OnceLock` for the binary — a second copy races `set_var`.
    pub(crate) fn fake_home() {
        use std::sync::OnceLock;
        static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
        HOME.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("pf-settings-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let var = if cfg!(windows) { "APPDATA" } else { "HOME" };
            // SAFETY: runs at most once, inside `get_or_init` — concurrent `fake_home`
            // callers block until it returns, and nothing else in this binary mutates
            // the env var.
            unsafe { std::env::set_var(var, &dir) };
            dir
        });
    }

    /// Draw once so hit-testing reads real strip/list geometry.
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

    #[test]
    fn a_press_on_a_row_focuses_and_cycles_it() {
        fake_home();
        let mut s = SettingsScreen::with_profiles(Vec::new());
        rendered(&mut s);
        let first = s.list.row_rect(0).expect("the list drew its rows");
        let (mut settings, pads) = ctx_parts();
        settings.save(); // seat the fake HOME file — `apply_row` rebases on it
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

    /// Speaker row: stored `"mix"` reads Off; a step writes only `"pad"` / `"off"`.
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
        assert!(ctx.settings.pad_haptics);
        assert_eq!(ctx.settings.pad_speaker, "pad");
        assert!(adjust(RowId::PadHaptics, 1, true, &mut ctx));
        assert!(!ctx.settings.pad_haptics);
        assert!(adjust(RowId::PadSpeaker, 1, true, &mut ctx));
        assert_eq!(ctx.settings.pad_speaker, "off");
        assert!(adjust(RowId::PadSpeaker, 1, true, &mut ctx));
        assert_eq!(ctx.settings.pad_speaker, "pad");
        ctx.settings.pad_speaker = "mix".into();
        assert!(adjust(RowId::PadSpeaker, 1, true, &mut ctx));
        assert_eq!(ctx.settings.pad_speaker, "pad");
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
        // Native (index 0): left refuses; right is Match window, then sizes.
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
        assert!(adjust(RowId::Resolution, -1, false, &mut ctx));
        assert!(ctx.settings.match_window);
        assert!(adjust(RowId::Resolution, -1, false, &mut ctx));
        assert!(!ctx.settings.match_window);
        assert_eq!(ctx.settings.width, 0, "back to Native");
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

    #[test]
    fn bitrate_dims_under_pyrowave() {
        let (mut settings, pads) = ctx_parts();
        settings.codec = "pyrowave".into();
        settings.bitrate_kbps = 80_000;
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
        assert!(!row_spec(RowId::Bitrate, &ctx, &[]).enabled);
        assert!(
            !adjust(RowId::Bitrate, 1, false, &mut ctx),
            "pyrowave = thud"
        );
        assert!(!adjust(RowId::Bitrate, 1, true, &mut ctx), "A too");
        assert_eq!(ctx.settings.bitrate_kbps, 80_000, "the stored rate is kept");

        ctx.settings.codec = "hevc".into();
        assert!(row_spec(RowId::Bitrate, &ctx, &[]).enabled);
        assert!(adjust(RowId::Bitrate, 1, false, &mut ctx));
    }

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
        assert!(
            !adjust(RowId::SmoothBuffer, 1, false, &mut ctx),
            "latency intent = thud"
        );
        assert_eq!(ctx.settings.smooth_buffer, 0, "and nothing was written");

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

    /// Cursor past a list that shrank: pull back, do not index. Another writer can
    /// flip presentation intent while this screen is open.
    #[test]
    fn a_shrinking_list_pulls_the_cursor_back() {
        // Seat the FILE with the shrunken list: `apply_row` rebases on it.
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
        s.list.cursor = s.row_ids(&ctx).len() - 1;
        let parked = s.list.cursor;
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
        assert!(
            !adjust(RowId::Touch, -1, false, &mut ctx),
            "already first = thud"
        );
        assert!(adjust(RowId::Touch, 1, false, &mut ctx));
        assert_eq!(ctx.settings.touch_mode, "pointer");
        assert!(adjust(RowId::Touch, 1, false, &mut ctx));
        assert_eq!(ctx.settings.touch_mode, "touch");
        assert!(!adjust(RowId::Touch, 1, false, &mut ctx), "last = thud");
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
        assert!(
            !adjust(RowId::Mouse, -1, false, &mut ctx),
            "already first = thud"
        );
        assert!(adjust(RowId::Mouse, 1, false, &mut ctx));
        assert_eq!(ctx.settings.mouse_mode, "desktop");
        assert!(!adjust(RowId::Mouse, 1, false, &mut ctx), "last = thud");
        assert!(adjust(RowId::Mouse, 1, true, &mut ctx));
        assert_eq!(ctx.settings.mouse_mode, "capture");
    }

    /// Off-ladder must not snap to Automatic (index 0). Step to the neighbour.
    #[test]
    fn an_off_ladder_rate_steps_to_its_neighbour() {
        let (mut settings, pads) = ctx_parts();
        settings.bitrate_kbps = 12_345;
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
        assert_eq!(ctx.settings.bitrate_kbps, 15_000, "the rung above");
        ctx.settings.bitrate_kbps = 12_345;
        assert!(adjust(RowId::Bitrate, -1, false, &mut ctx));
        assert_eq!(ctx.settings.bitrate_kbps, 12_000, "the rung below");
        ctx.settings.bitrate_kbps = 2_000_000;
        assert!(!adjust(RowId::Bitrate, 1, false, &mut ctx), "the ceiling");
        ctx.settings.bitrate_kbps = 5_000;
        assert!(adjust(RowId::Bitrate, -1, false, &mut ctx));
        assert_eq!(ctx.settings.bitrate_kbps, 4_000);
    }

    #[test]
    fn a_typed_bitrate_is_stored_and_clamped() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        // Snapshot, not the file store: this test saves.
        let store = crate::store::SnapshotStore::new(settings.clone(), Vec::new());
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: &store,
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        let mut s = SettingsScreen::with_profiles(Vec::new());
        let mut fx = Outbox::default();
        let ids = s.row_ids(&ctx);
        s.list.cursor = ids
            .iter()
            .position(|id| *id == RowId::Bitrate)
            .expect("the bitrate row");
        s.menu(MenuEvent::Secondary, &mut ctx, &mut fx);
        assert!(s.editing(), "Y opens the field");
        s.text_input("13x7"); // digits only: 'x' is refused
        assert!(s.edit_key(crate::input::Key::Return, &mut ctx));
        assert!(!s.editing(), "Return closes it");
        assert_eq!(ctx.settings.bitrate_kbps, 137_000);

        s.menu(MenuEvent::Secondary, &mut ctx, &mut fx);
        s.text_input("99999"); // four digits; the fifth is refused
        assert!(s.edit_key(crate::input::Key::Return, &mut ctx));
        assert_eq!(
            ctx.settings.bitrate_kbps, 2_000_000,
            "clamped to the ceiling"
        );

        s.menu(MenuEvent::Secondary, &mut ctx, &mut fx);
        assert!(s.edit_key(crate::input::Key::Return, &mut ctx));
        assert_eq!(ctx.settings.bitrate_kbps, 2_000_000, "left alone");

        s.list.cursor = 0;
        s.menu(MenuEvent::Secondary, &mut ctx, &mut fx);
        assert!(!s.editing());
    }

    #[test]
    fn rates_read_in_the_biggest_round_unit() {
        assert_eq!(bitrate_label(20_000), "20 Mbps");
        assert_eq!(bitrate_label(12_500), "12.5 Mbps");
        assert_eq!(bitrate_label(1_000_000), "1 Gbps");
        assert_eq!(bitrate_label(1_500_000), "1.5 Gbps");
        assert_eq!(bitrate_label(2_000_000), "2 Gbps");
    }

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
            actions: Vec::new(),
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

        s.list.cursor = 0;
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

    #[test]
    fn the_quick_actions_row_opens_the_editor_and_steps_nothing() {
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
        let row = row_spec(RowId::QuickActions, &ctx, &[]);
        assert!(row.value.is_none(), "an action row");
        assert_eq!(row.label, "Quick actions");
        assert!(!adjust(RowId::QuickActions, 1, false, &mut ctx));
        assert!(
            ctx.settings.overlay_actions.is_empty(),
            "the row itself never writes the blob"
        );
        assert!(TABS
            .iter()
            .any(|(tab, rows)| *tab == "Input" && rows.contains(&RowId::QuickActions)));
    }

    #[test]
    fn platform_row_split_hides_only_the_other_platforms_concepts() {
        use crate::platform::Platform;
        let all: Vec<RowId> = TABS
            .iter()
            .flat_map(|(_, rows)| rows.iter().copied())
            .collect();
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
                RowId::TenBitSdr,
                RowId::Vsync,
                RowId::AllowVrr,
                RowId::Shortcuts,
                RowId::Fullscreen,
            ]
        );
        assert!(all
            .iter()
            .all(|id| row_on(*id, Platform::Desktop) || row_on(*id, Platform::Android)));
    }

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
            let mut after = ctx.settings.clone();
            after.extra = before.extra.clone();
            assert_eq!(after, before);
        });
    }

    #[test]
    fn console_off_switch_needs_a_fallback_ui() {
        with_ctx(|ctx| {
            ctx.platform = crate::platform::Platform::Android;
            assert!(
                !row_applies(RowId::GamepadUi, ctx),
                "a TV offers no off switch"
            );
            assert!(!row_applies(RowId::GamepadUiMode, ctx));
            ctx.fallback_ui = true;
            assert!(row_applies(RowId::GamepadUi, ctx));
            assert!(row_applies(RowId::GamepadUiMode, ctx));
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
        assert_eq!(seen.len(), 49, "{seen:?}");
        assert!(seen.contains(&RowId::FollowOsTheme));
        assert!(seen.contains(&RowId::Palette));
        assert!(seen.contains(&RowId::ReduceMotion));
        assert!(seen.contains(&RowId::ReduceUiResolution));
        assert!(seen.contains(&RowId::AudioFormat));
        assert!(TABS[PROFILES_TAB].1.is_empty());
        assert_eq!(TABS[PROFILES_TAB].0, "Profiles");
    }

    /// Only test that touches the process-wide `os_theme` slot. A sibling races
    /// under libtest. Leaves the slot cleared.
    #[test]
    fn the_follow_system_row_exists_only_where_a_theme_is_published() {
        with_ctx(|ctx| {
            assert!(
                !row_applies(RowId::FollowOsTheme, ctx),
                "no publisher, no row"
            );
            assert!(row_applies(RowId::Palette, ctx));

            let t = crate::os_theme::OsTheme {
                light: false,
                background: (0.02, 0.04, 0.12),
                foreground: (1.0, 0.81, 0.68),
                accent: (0.49, 0.51, 0.85),
            };
            crate::os_theme::set_os_theme(Some(t));
            let rev = crate::os_theme::os_theme().0;
            crate::os_theme::set_os_theme(Some(t));
            assert_eq!(
                crate::os_theme::os_theme().0,
                rev,
                "an unchanged publish is free"
            );

            assert!(row_applies(RowId::FollowOsTheme, ctx));
            assert!(
                !row_applies(RowId::Palette, ctx),
                "ruled by the system theme"
            );

            ctx.settings.follow_os_theme = false;
            assert!(row_applies(RowId::Palette, ctx));

            crate::os_theme::set_os_theme(None);
            assert!(!row_applies(RowId::FollowOsTheme, ctx));
        });
    }

    /// Off by default: this key decides where a deep link lands, so an install
    /// that never opens this screen must keep the shelf it has.
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
        s.list.cursor = 3; // Stream / Bitrate
        s.menu(MenuEvent::JumpForward, &mut ctx, &mut fx);
        assert_eq!(s.tab, 1);
        assert_eq!(s.list.cursor, 0, "a fresh tab starts at its first row");
        s.list.cursor = 2; // Video / 10-bit HDR
        s.menu(MenuEvent::JumpBack, &mut ctx, &mut fx);
        assert_eq!((s.tab, s.list.cursor), (0, 3), "Stream kept its place");
        s.menu(MenuEvent::JumpBack, &mut ctx, &mut fx);
        assert_eq!(s.tab, PROFILES_TAB);
        assert_eq!(s.list.cursor, 0);
        s.menu(MenuEvent::JumpForward, &mut ctx, &mut fx);
        assert_eq!(s.tab, 0);
        assert!(fx.nav.is_none() && fx.cmds.is_empty());
    }

    /// TV remotes have no shoulders and no Tab key: Up from row 0 focuses the strip.
    #[test]
    fn dpad_alone_reaches_every_tab() {
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
        assert_eq!(s.list.cursor, 0);
        s.menu(MenuEvent::Move(MenuDir::Up), &mut ctx, &mut fx);
        assert!(s.strip_focus, "Up from the top row lands on the strip");
        s.menu(MenuEvent::Move(MenuDir::Right), &mut ctx, &mut fx);
        assert_eq!(s.tab, 1);
        assert!(s.strip_focus, "switching keeps the strip focused");
        s.menu(MenuEvent::Move(MenuDir::Left), &mut ctx, &mut fx);
        s.menu(MenuEvent::Move(MenuDir::Left), &mut ctx, &mut fx);
        assert_eq!(s.tab, PROFILES_TAB, "the strip wraps like the shoulders do");
        s.menu(MenuEvent::Move(MenuDir::Down), &mut ctx, &mut fx);
        assert!(!s.strip_focus, "Down drops back into the list");
        s.menu(MenuEvent::Move(MenuDir::Down), &mut ctx, &mut fx);
        assert!(!s.strip_focus);
        assert!(fx.nav.is_none() && fx.cmds.is_empty());
    }

    /// Assert against `audio_format` constants so a spelling change there reds this
    /// instead of writing a key nobody reads. Dim under surround: see [`row_spec`].
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
        assert!(adjust(RowId::AudioFormat, 1, true, &mut ctx));
        assert_eq!(ctx.settings.audio_format, AUDIO_FORMAT_OPUS);

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
        assert!(s.row_ids(&ctx).contains(&RowId::AudioFormat));
        ctx.settings.audio_channels = 2;
        assert!(row_spec(RowId::AudioFormat, &ctx, &[]).enabled);

        ctx.settings.audio_format = AUDIO_FORMAT_OPUS.into();
        let opus = row_spec(RowId::AudioFormat, &ctx, &[]).value;
        assert!(opus.is_some());
        ctx.settings.audio_format = "lossless192".into();
        assert_eq!(row_spec(RowId::AudioFormat, &ctx, &[]).value, opus);
    }

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
        ctx.settings.ui_palette = crate::library::PALETTES
            .last()
            .expect("non-empty")
            .id
            .to_string();
        assert!(adjust(RowId::Palette, 1, true, &mut ctx));
        assert_eq!(ctx.settings.ui_palette, "violet");
        ctx.settings.ui_palette = "chartreuse".into();
        assert_eq!(
            row_spec(RowId::Palette, &ctx, &[]).value.as_deref(),
            Some("Violet"),
            "an unknown palette reads as the default it actually draws"
        );
    }
}
