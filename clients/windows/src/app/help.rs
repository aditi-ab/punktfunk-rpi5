//! The Shortcuts screen: a short note on the in-stream capture model plus a reference of the
//! keyboard shortcuts — reached from the Shortcuts button on the host list. The Windows
//! counterpart of the GTK client's Keyboard Shortcuts window; the bindings themselves live in
//! the session window, so both clients document the same set.

use super::lucide;
use super::style::*;
use super::Screen;
use windows_reactor::*;

/// The in-stream keyboard shortcuts, in the GTK Shortcuts window's order: the chord, then what it
/// does. Read-only — the keyboard bindings live in the session window (`pf-presenter`'s run
/// loop), the controller chord in its gamepad service.
const STREAM_SHORTCUTS: &[(&str, &str)] = &[
    ("F11 / Alt+Enter", "Toggle fullscreen"),
    (
        "Ctrl+Alt+Shift+Q",
        "Release captured input (click the stream to recapture)",
    ),
    ("Ctrl+Alt+Shift+D", "Disconnect"),
    (
        "Ctrl+Alt+Shift+S",
        "Cycle the statistics overlay (off \u{00B7} compact \u{00B7} normal \u{00B7} detailed)",
    ),
    (
        "Ctrl+Alt+Shift+V",
        "Mute or unmute your microphone (only while the stream sends one)",
    ),
    (
        "LB+RB+Start+Back",
        "Controller: release input / leave fullscreen \u{2014} hold to disconnect",
    ),
];

/// A subtle key-cap chip for the shortcuts reference — the chord on a filled, bordered pill.
fn key_chip(keys: &str) -> Element {
    border(text_block(keys).font_size(12.0).semibold())
        .background(ThemeRef::SubtleFill)
        .border_brush(ThemeRef::CardStroke)
        .border_thickness(uniform(1.0))
        .corner_radius(6.0)
        .padding(edges(8.0, 3.0, 8.0, 3.0))
        .horizontal_alignment(HorizontalAlignment::Left)
        .into()
}

/// A read-only reference card listing the in-stream keyboard shortcuts. One grid, chord chip then
/// action, so the actions line up across rows.
fn shortcuts_reference() -> Element {
    let mut children: Vec<Element> = Vec::new();
    for (i, (keys, action)) in STREAM_SHORTCUTS.iter().enumerate() {
        let row = i as i32;
        children.push(key_chip(keys).grid_row(row).grid_column(0));
        let action_cell: Element = text_block(*action)
            .wrap()
            .foreground(ThemeRef::SecondaryText)
            .vertical_alignment(VerticalAlignment::Center)
            .into();
        children.push(action_cell.grid_row(row).grid_column(1));
    }
    let table = grid(children)
        .columns([GridLength::Auto, GridLength::Star(1.0)])
        .rows(vec![GridLength::Auto; STREAM_SHORTCUTS.len()])
        .column_spacing(12.0)
        .row_spacing(6.0);
    card(vstack((
        text_block("In-stream keyboard shortcuts")
            .semibold()
            .margin(edges(0.0, 0.0, 0.0, 8.0)),
        table,
    )))
    .into()
}

/// The Shortcuts screen: a `page`-column with a Back button to the host list, an intro card on
/// the capture model, and the shortcuts reference. Hook-free — called inline from `root` like
/// the other static screens.
pub(crate) fn help_page(set_screen: &AsyncSetState<Screen>) -> Element {
    let back_btn = button("Back")
        .accent()
        .icon(lucide::icon("arrow-left"))
        .on_click({
            let ss = set_screen.clone();
            move || ss.call(Screen::Hosts)
        });

    let intro = card(
        vstack((
            text_block("During a stream").font_size(15.0).semibold(),
            text_block(
                "Click the stream to capture your mouse and keyboard \u{2014} the shortcuts below \
                 then work while you play. Release capture to hand the cursor back to this \
                 computer, and click the stream again to retake it.",
            )
            .font_size(12.0)
            .wrap()
            .foreground(ThemeRef::SecondaryText),
        ))
        .spacing(8.0),
    );

    page(vec![
        page_header("Shortcuts", back_btn),
        intro.into(),
        shortcuts_reference(),
    ])
}
