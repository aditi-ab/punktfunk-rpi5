//! Preferences dialog: stream mode, bitrate, host compositor, gamepad type, microphone,
//! capture behavior. Written back to disk when the dialog closes.

use crate::trust::Settings;
use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

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
const GAMEPADS: &[&str] = &["auto", "xbox360", "dualsense", "xboxone", "dualshock4"];
const COMPOSITORS: &[&str] = &["auto", "kwin", "wlroots", "mutter", "gamescope"];

pub fn show(
    parent: &impl IsA<gtk::Widget>,
    settings: Rc<RefCell<Settings>>,
    gamepads: &crate::gamepad::GamepadService,
) {
    let page = adw::PreferencesPage::new();

    let stream = adw::PreferencesGroup::builder().title("Stream").build();
    let res_names: Vec<String> = RESOLUTIONS
        .iter()
        .map(|&(w, h)| {
            if w == 0 {
                "Native display".to_string()
            } else {
                format!("{w} × {h}")
            }
        })
        .collect();
    let res_row = adw::ComboRow::builder()
        .title("Resolution")
        .subtitle("The host creates a virtual output at exactly this size")
        .model(&gtk::StringList::new(
            &res_names.iter().map(String::as_str).collect::<Vec<_>>(),
        ))
        .build();
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
    let hz_row = adw::ComboRow::builder()
        .title("Refresh rate")
        .model(&gtk::StringList::new(
            &hz_names.iter().map(String::as_str).collect::<Vec<_>>(),
        ))
        .build();
    let bitrate_row = adw::SpinRow::with_range(0.0, 3000.0, 5.0);
    bitrate_row.set_title("Bitrate");
    bitrate_row.set_subtitle("Mbit/s · 0 = host default · run a speed test before going high");
    let compositor_row = adw::ComboRow::builder()
        .title("Host compositor")
        .subtitle("Advisory — the host falls back to auto-detect when unavailable")
        .model(&gtk::StringList::new(&[
            "Automatic",
            "KWin",
            "wlroots (Sway/Hyprland)",
            "Mutter (GNOME)",
            "gamescope",
        ]))
        .build();
    stream.add(&res_row);
    stream.add(&hz_row);
    stream.add(&bitrate_row);
    stream.add(&compositor_row);

    let input = adw::PreferencesGroup::builder().title("Input").build();
    // Which physical controller forwards as pad 0: automatic = the most recently
    // connected; pinning survives until the app exits (Swift parity).
    let pads = gamepads.pads();
    let mut pad_names = vec!["Automatic (most recent)".to_string()];
    pad_names.extend(pads.iter().map(|p| {
        let kind = p.kind_label();
        if kind.is_empty() {
            p.name.clone()
        } else {
            format!("{} · {kind}", p.name)
        }
    }));
    let forward_row = adw::ComboRow::builder()
        .title("Forwarded controller")
        .subtitle(if pads.is_empty() {
            "No controllers detected"
        } else {
            "Exactly one controller is forwarded to the host"
        })
        .model(&gtk::StringList::new(
            &pad_names.iter().map(String::as_str).collect::<Vec<_>>(),
        ))
        .build();
    let pinned_i = gamepads
        .pinned()
        .and_then(|id| pads.iter().position(|p| p.id == id))
        .map_or(0, |i| i + 1);
    forward_row.set_selected(pinned_i as u32);
    {
        let svc = gamepads.clone();
        let ids: Vec<u32> = pads.iter().map(|p| p.id).collect();
        forward_row.connect_selected_notify(move |row| {
            let sel = row.selected() as usize;
            svc.set_pinned(if sel == 0 {
                None
            } else {
                ids.get(sel - 1).copied()
            });
        });
    }
    let pad_row = adw::ComboRow::builder()
        .title("Gamepad type")
        .subtitle("The virtual pad the host creates — Automatic matches the physical pad")
        .model(&gtk::StringList::new(&[
            "Automatic",
            "Xbox 360",
            "DualSense",
            "Xbox One",
            "DualShock 4",
        ]))
        .build();
    let inhibit_row = adw::SwitchRow::builder()
        .title("Capture system shortcuts")
        .subtitle("Forward Alt+Tab, Super, … to the host while input is captured")
        .build();
    input.add(&forward_row);
    input.add(&pad_row);
    input.add(&inhibit_row);

    let audio = adw::PreferencesGroup::builder().title("Audio").build();
    let mic_row = adw::SwitchRow::builder()
        .title("Stream microphone")
        .subtitle("Send the default input device to the host's virtual microphone")
        .build();
    audio.add(&mic_row);

    page.add(&stream);
    page.add(&input);
    page.add(&audio);

    // Seed from the current settings.
    {
        let s = settings.borrow();
        let res_i = RESOLUTIONS
            .iter()
            .position(|&(w, h)| w == s.width && h == s.height)
            .unwrap_or(0);
        res_row.set_selected(res_i as u32);
        let hz_i = REFRESH.iter().position(|&r| r == s.refresh_hz).unwrap_or(0);
        hz_row.set_selected(hz_i as u32);
        bitrate_row.set_value(f64::from(s.bitrate_kbps) / 1000.0);
        let pad_i = GAMEPADS.iter().position(|&g| g == s.gamepad).unwrap_or(0);
        pad_row.set_selected(pad_i as u32);
        let comp_i = COMPOSITORS
            .iter()
            .position(|&c| c == s.compositor)
            .unwrap_or(0);
        compositor_row.set_selected(comp_i as u32);
        inhibit_row.set_active(s.inhibit_shortcuts);
        mic_row.set_active(s.mic_enabled);
    }

    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Preferences");
    dialog.add(&page);
    dialog.connect_closed(move |_| {
        let mut s = settings.borrow_mut();
        let (w, h) = RESOLUTIONS[(res_row.selected() as usize).min(RESOLUTIONS.len() - 1)];
        (s.width, s.height) = (w, h);
        s.refresh_hz = REFRESH[(hz_row.selected() as usize).min(REFRESH.len() - 1)];
        s.bitrate_kbps = (bitrate_row.value() * 1000.0) as u32;
        s.gamepad = GAMEPADS[(pad_row.selected() as usize).min(GAMEPADS.len() - 1)].to_string();
        s.compositor = COMPOSITORS[(compositor_row.selected() as usize).min(COMPOSITORS.len() - 1)]
            .to_string();
        s.inhibit_shortcuts = inhibit_row.is_active();
        s.mic_enabled = mic_row.is_active();
        s.save();
    });
    dialog.present(Some(parent));
}
