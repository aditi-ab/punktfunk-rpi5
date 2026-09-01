//! The TUI driven by injected key events, with the frames pinned as goldens.
//!
//! This is what S1 bought by hand-rolling the widget: the screen a user would be looking at is
//! a value a test can assert on, so the PTY smoke only has to prove the real terminal agrees
//! rather than being the only place the rendering is exercised at all.
//!
//! Rendered with `Colors::None` so the goldens are readable text — the escape sequences have
//! their own tests in `ui::theme`.

use punktfunk_setup::choices::{Choices, Pins};
use punktfunk_setup::demo;
use punktfunk_setup::ui::logo::MARK_TEXT_ROWS;
use punktfunk_setup::ui::summary::{Screen, Step};
use punktfunk_setup::ui::term::{Key, ScriptedTerm, Terminal};
use punktfunk_setup::ui::theme::{Caps, Colors};
use punktfunk_setup::ui::tui::Tui;

fn caps() -> Caps {
    Caps {
        tty: true,
        colors: Colors::None,
        width: 100,
    }
}

/// Drive the settings screen with a key script; return the last frame and how it ended.
fn drive(preset: &str, keys: &[Key]) -> (String, Step, Screen) {
    let (frame, step, screen, _) = drive_all(preset, keys);
    (frame, step, screen)
}

/// As `drive`, plus every frame that was written — a frame the loop later cleared (a row
/// editor backed out of) is only visible here.
fn drive_all(preset: &str, keys: &[Key]) -> (String, Step, Screen, Vec<String>) {
    let facts = demo::preset(preset).expect("preset");
    let choices = Choices::derive(&facts, &Pins::default());
    let mut screen = Screen::new(facts, choices);
    let mut term = ScriptedTerm::new(keys);
    let step = {
        let tui = Tui::new(&mut term as &mut dyn Terminal, caps(), 0);
        tui.settings(&mut screen, 0)
    };
    (term.screen().to_string(), step, screen, term.frames.clone())
}

fn golden(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.txt"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("no golden for {name} — run UPDATE_GOLDEN=1"));
    assert_eq!(
        actual, want,
        "golden {name} changed (UPDATE_GOLDEN=1 to accept)"
    );
}

#[test]
fn the_settings_screen_as_the_user_first_sees_it() {
    let (frame, step, _) = drive("arch-fresh", &[Key::Enter]);
    golden("tui-arch-fresh", &frame);
    assert!(matches!(step, Step::Run(_)), "Enter on arrival installs");
}

#[test]
fn a_couch_box_shows_its_derived_defaults() {
    let (frame, _, _) = drive("bazzite-couch", &[Key::Enter]);
    golden("tui-bazzite-couch", &frame);
}

/// Omarchy's own options are rows on this screen, not a second round of questions from
/// `punktfunk-omarchy setup` after this one has finished.
#[test]
fn an_omarchy_box_asks_for_its_own_options_here() {
    let (frame, _, _) = drive("omarchy", &[Key::Enter]);
    golden("tui-omarchy", &frame);
    for row in [
        "Console certificate",
        "Desktop notifications",
        "Keep the screen awake",
        "Match the Omarchy theme",
    ] {
        assert!(frame.contains(row), "{row} is missing from the screen");
    }
}

/// Manage mode: the screen re-titles and grows an Uninstall row.
#[test]
fn an_installed_box_shows_the_manage_screen() {
    let (frame, _, _) = drive("arch-canary-installed", &[Key::Enter]);
    golden("tui-manage", &frame);
    assert!(frame.contains("Uninstall"));
    assert!(frame.contains("Apply these changes"));
}

/// Enter on a row opens its editor; an exhausted script then backs out of both.
#[test]
fn the_cursor_lands_on_the_row_it_was_moved_to() {
    let (frame, step, screen) = drive("arch-fresh", &[Key::Down, Key::Down]);
    golden("tui-cursor-on-channel", &frame);
    assert_eq!(screen.cursor, 2);
    assert_eq!(step, Step::Cancel);
}

/// Editing a row opens the radio list, then folds the answer back into the plan.
#[test]
fn editing_moonlight_compat_adds_the_firewall_step() {
    // Down×4 lands on Moonlight compat; Enter opens the editor, Up picks "yes", Enter accepts.
    // The cursor stays on the row it just edited, so walk back up to the action and install.
    let keys = [
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Enter,
        Key::Up,
        Key::Enter,
        Key::Up,
        Key::Up,
        Key::Up,
        Key::Up,
        Key::Enter,
    ];
    let (_, step, screen) = drive("omarchy", &keys);
    assert!(screen.choices.gamestream, "the edit did not stick");
    assert!(
        screen
            .plan()
            .commands()
            .iter()
            .any(|c| c.contains("punktfunk-gamestream")),
        "the plan was not re-resolved: {:?}",
        screen.plan().commands()
    );
    assert!(
        matches!(step, Step::Run(_)),
        "the walk back to the action did not install: {step:?}"
    );
}

/// The editor carries the sh prompt's why-text, so accepting a default never hides the grant.
#[test]
fn the_row_editor_frame_names_the_grant() {
    let keys = [Key::Down, Key::Down, Key::Down, Key::Enter];
    let (_, _, _, frames) = drive_all("arch-fresh", &keys);
    let editor = frames
        .iter()
        .rev()
        .find(|f| f.contains('●'))
        .expect("no radio list was ever drawn");
    golden("tui-editor-group", editor);
    assert!(
        editor.contains("usbip attach"),
        "the editor hid what the row grants"
    );
}

/// The mark is part of the settings frame, so picking Client greys the host circle under the
/// cursor instead of leaving a banner that disagrees with the row below it.
#[test]
fn the_mark_mutes_the_half_that_is_not_being_installed() {
    let colour = Caps {
        tty: true,
        colors: Colors::Truecolor,
        width: 100,
    };
    let render = |host: bool, client: bool| {
        let facts = demo::preset("arch-fresh").expect("preset");
        let mut choices = Choices::derive(&facts, &Pins::default());
        choices.components.host = host;
        choices.components.client = client;
        let mut screen = Screen::new(facts, choices);
        let mut term = ScriptedTerm::new(&[Key::Enter]);
        {
            let tui = Tui::new(&mut term as &mut dyn Terminal, colour, 0);
            tui.settings(&mut screen, 0);
        }
        term.screen().to_string()
    };

    let host_only = render(true, false);
    let both = render(true, true);
    let client_only = render(false, true);

    assert!(
        host_only.contains("\x1b[48;2;"),
        "the frame carries no mark at all"
    );
    assert_ne!(
        host_only, both,
        "selecting the client changed nothing in the mark"
    );
    assert_ne!(client_only, both);
    assert_ne!(host_only, client_only);

    // The mark paints backgrounds; the same colour appears as a foreground in the TUI's
    // highlight, so the escape has to be the background one to mean the lens.
    let mark_of = |frame: &str| {
        frame
            .lines()
            .take(MARK_TEXT_ROWS)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let lens = "\x1b[48;2;210;201;251m";
    assert!(
        mark_of(&both).contains(lens),
        "both installed should light the lens"
    );
    assert!(
        !mark_of(&host_only).contains(lens),
        "a host-only install must not light the lens"
    );
}

/// Visible columns, ignoring the escape sequences.
fn columns(line: &str) -> usize {
    let mut cols = 0;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            cols += 1;
        }
    }
    cols
}

/// A line wider than the terminal wraps, and a wrapped line makes `clear_last_lines` rewind
/// fewer rows than the frame drew — so every keystroke leaves another stripe behind.
#[test]
fn no_frame_line_is_wider_than_the_terminal() {
    for width in [30u16, 40, 60, 80, 120] {
        let caps = Caps {
            tty: true,
            colors: Colors::Truecolor,
            width,
        };
        let facts = demo::preset("bazzite-couch").expect("preset");
        let choices = Choices::derive(&facts, &Pins::default());
        let mut screen = Screen::new(facts, choices);
        let mut term = ScriptedTerm::new(&[Key::Enter]);
        term.width = width;
        {
            let tui = Tui::new(&mut term as &mut dyn Terminal, caps, 0);
            tui.settings(&mut screen, 0);
        }
        for line in term.screen().lines() {
            assert!(
                columns(line) <= usize::from(width),
                "at width {width} a line ran to {} columns: {line:?}",
                columns(line)
            );
        }
    }
}

/// Editing a row used to leave a collapsed answer line behind, so opening the same editor
/// twice pushed the screen down twice. The row already shows the answer.
#[test]
fn repeated_edits_do_not_pile_up_lines() {
    let once = drive(
        "arch-fresh",
        &[Key::Down, Key::Enter, Key::Enter, Key::Char('q')],
    )
    .0;
    let twice = drive(
        "arch-fresh",
        &[
            Key::Down,
            Key::Enter,
            Key::Enter,
            Key::Enter,
            Key::Enter,
            Key::Char('q'),
        ],
    )
    .0;
    assert_eq!(
        once.lines().count(),
        twice.lines().count(),
        "a second edit left an extra line on screen"
    );
}

#[test]
fn q_cancels_without_running_anything() {
    let (_, step, _) = drive("arch-fresh", &[Key::Char('q')]);
    assert_eq!(step, Step::Cancel);
}

/// An exhausted key script must end the loop, not spin it.
#[test]
fn a_terminal_that_stops_answering_ends_the_screen() {
    let (_, step, _) = drive("arch-fresh", &[]);
    assert_eq!(step, Step::Cancel);
}

#[test]
fn every_preset_renders_a_screen_without_panicking() {
    for name in demo::PRESETS {
        let (frame, _, _) = drive(name, &[Key::Enter]);
        assert!(!frame.is_empty(), "{name} rendered nothing");
    }
}
