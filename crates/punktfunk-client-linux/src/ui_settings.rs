//! Preferences dialog: stream mode, bitrate, gamepad type, capture behavior. Written
//! back to disk when the dialog closes.

use crate::trust::Settings;
use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

const RESOLUTIONS: &[(u32, u32)] = &[(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)];
const REFRESH: &[u32] = &[30, 60, 90, 120, 144, 165, 240];
const GAMEPADS: &[&str] = &["auto", "xbox360", "dualsense"];

pub fn show(parent: &impl IsA<gtk::Widget>, settings: Rc<RefCell<Settings>>) {
    let page = adw::PreferencesPage::new();

    let stream = adw::PreferencesGroup::builder().title("Stream").build();
    let res_names: Vec<String> = RESOLUTIONS
        .iter()
        .map(|(w, h)| format!("{w} × {h}"))
        .collect();
    let res_row = adw::ComboRow::builder()
        .title("Resolution")
        .subtitle("The host creates a virtual output at exactly this size")
        .model(&gtk::StringList::new(
            &res_names.iter().map(String::as_str).collect::<Vec<_>>(),
        ))
        .build();
    let hz_row = adw::ComboRow::builder()
        .title("Refresh rate")
        .model(&gtk::StringList::new(
            &REFRESH
                .iter()
                .map(|r| format!("{r} Hz"))
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        ))
        .build();
    let bitrate_row = adw::SpinRow::with_range(0.0, 500.0, 5.0);
    bitrate_row.set_title("Bitrate");
    bitrate_row.set_subtitle("Mbit/s · 0 = host default");
    stream.add(&res_row);
    stream.add(&hz_row);
    stream.add(&bitrate_row);

    let input = adw::PreferencesGroup::builder().title("Input").build();
    let pad_row = adw::ComboRow::builder()
        .title("Gamepad type")
        .subtitle("The virtual pad the host creates (DualSense needs a Linux host)")
        .model(&gtk::StringList::new(&["Auto", "Xbox 360", "DualSense"]))
        .build();
    let inhibit_row = adw::SwitchRow::builder()
        .title("Capture system shortcuts")
        .subtitle("Forward Alt+Tab, Super, … to the host while streaming")
        .build();
    input.add(&pad_row);
    input.add(&inhibit_row);

    page.add(&stream);
    page.add(&input);

    // Seed from the current settings.
    {
        let s = settings.borrow();
        let res_i = RESOLUTIONS
            .iter()
            .position(|&(w, h)| w == s.width && h == s.height)
            .unwrap_or(1);
        res_row.set_selected(res_i as u32);
        let hz_i = REFRESH.iter().position(|&r| r == s.refresh_hz).unwrap_or(1);
        hz_row.set_selected(hz_i as u32);
        bitrate_row.set_value(f64::from(s.bitrate_kbps) / 1000.0);
        let pad_i = GAMEPADS.iter().position(|&g| g == s.gamepad).unwrap_or(0);
        pad_row.set_selected(pad_i as u32);
        inhibit_row.set_active(s.inhibit_shortcuts);
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
        s.inhibit_shortcuts = inhibit_row.is_active();
        s.save();
    });
    dialog.present(Some(parent));
}
