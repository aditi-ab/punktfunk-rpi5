use super::*;
use crate::model::WakeStatus;
use crate::screens::home::HomeScreen;
use crate::screens::library::LibraryScreen;
use punktfunk_core::config::GamepadPref;

/// The screen-transition contract, against the shared vectors. Every client re-implements this
/// motion in its own animation system, so the numbers exist in three places and drifted in two of
/// them before this test.
///
/// The EASING is sampled rather than compared as control points, and that is the point of it:
/// this side is the analytic `1 − (1−t)³`, Android reproduces it exactly, and SwiftUI can only
/// approximate it with a Bézier. Worse, two different Béziers are published under the name
/// "easeOutCubic" — `(0.215, 0.61, 0.355, 1)` and `(0.33, 1, 0.68, 1)` — and neither IS this
/// curve; they differ from it by up to ~0.08 at the midpoint, which is visible on a 260 ms
/// transition. Samples with a tolerance are the only form all three runtimes can meet.
#[test]
fn motion_matches_the_shared_vectors() {
    let raw = include_str!("../../../../clients/shared/console-vectors.json");
    let file: serde_json::Value =
        serde_json::from_str(raw).expect("console-vectors.json must parse");
    let motion = &file["motion"];
    let want_s = motion["transition_s"].as_f64().expect("transition_s");
    assert!(
        (TRANSITION_S - want_s).abs() < 1e-9,
        "TRANSITION_S is {TRANSITION_S}, vectors say {want_s}"
    );

    let curve = &motion["ease_out_cubic"];
    let tol = curve["tolerance"].as_f64().expect("tolerance");
    let samples = curve["samples"].as_array().expect("samples");
    assert!(
        samples.len() >= 5,
        "the curve needs enough samples to pin it"
    );
    for s in samples {
        let t = s["t"].as_f64().expect("t");
        let want = s["p"].as_f64().expect("p");
        let got = crate::anim::ease_out_cubic(t);
        assert!(
            (got - want).abs() <= tol,
            "ease_out_cubic({t}) is {got}, vectors say {want} (±{tol})"
        );
    }
}

/// Point the settings/known-hosts stores at a throwaway HOME — the settings screen
/// SAVES on adjust, and a test must never write the developer's real config.
fn fake_home() {
    use std::sync::OnceLock;
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    let dir = HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("pf-console-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("HOME", &dir);
        dir.clone()
    });
    std::env::set_var("HOME", dir);
}

fn hosts() -> Vec<HostRow> {
    let base = HostRow {
        key: String::new(),
        name: String::new(),
        addr: "10.0.0.20".into(),
        port: 9777,
        fp_hex: String::new(),
        paired: false,
        saved: true,
        online: false,
        mgmt_port: 47990,
        can_wake: false,
        last_used: None,
        os: String::new(),
        pin: None,
        bound_profile: None,
    };
    vec![
        HostRow {
            key: "aa11".into(),
            name: "Living Room PC".into(),
            fp_hex: "aa11".into(),
            paired: true,
            online: true,
            last_used: Some(1),
            ..base.clone()
        },
        HostRow {
            key: "bb22".into(),
            name: "Office Tower".into(),
            addr: "10.0.0.21".into(),
            fp_hex: "bb22".into(),
            paired: true,
            can_wake: true,
            ..base.clone()
        },
        HostRow {
            key: "10.0.0.30:9777".into(),
            name: "steambox".into(),
            addr: "10.0.0.30".into(),
            saved: false,
            online: true,
            ..base
        },
    ]
}

fn shell(stack: Vec<Screen>) -> (Shell, ConsoleShared, LibraryShared) {
    fake_home();
    let console = ConsoleShared::default();
    console.set_hosts(hosts());
    let library = LibraryShared::default();
    let bus = ConsoleBus::default();
    let shell = Shell::new(
        console.clone(),
        library.clone(),
        bus,
        ConsoleOptions {
            device_name: "deck".into(),
            deck: false,
        },
        stack,
    )
    .unwrap();
    (shell, console, library)
}

/// The shell survives a full navigation lap (a smoke test over every screen's
/// input handling — no rendering, no GPU).
#[test]
fn navigation_lap() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    // Home → Settings (X), adjust something, back out.
    s.handle_menu(MenuEvent::Tertiary);
    assert_eq!(s.stack.len(), 2);
    finish_motion(&mut s);
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    s.handle_menu(MenuEvent::Back);
    finish_motion(&mut s);
    assert_eq!(s.stack.len(), 1);
    // Home → Library on the paired host (Y), then back.
    s.handle_menu(MenuEvent::Secondary);
    assert_eq!(s.stack.len(), 2);
    finish_motion(&mut s);
    s.handle_menu(MenuEvent::Back);
    finish_motion(&mut s);
    assert_eq!(s.stack.len(), 1);
    // B at the root quits.
    s.handle_menu(MenuEvent::Back);
    assert!(matches!(s.take_action(), Some(OverlayAction::Quit)));
}

#[test]
fn connect_flow_raises_launch_and_cancel() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.handle_menu(MenuEvent::Confirm); // paired+online host focused first
    assert!(matches!(
        s.take_action(),
        Some(OverlayAction::Launch { launch: None, .. })
    ));
    assert!(s.connecting.is_some());
    // While connecting: B cancels exactly once.
    s.handle_menu(MenuEvent::Back);
    assert!(matches!(
        s.take_action(),
        Some(OverlayAction::CancelConnect)
    ));
    s.handle_menu(MenuEvent::Back);
    assert!(s.take_action().is_none(), "cancel is idempotent");
    // The canceled dial ends silently.
    s.session_ended(None);
    assert!(s.connecting.is_none());
}

fn finish_motion(s: &mut Shell) {
    // Transitions block input; tests fast-forward them.
    s.motion = Motion::None;
}

#[test]
fn wake_gates_input_in_the_same_press() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    // Focus "Office Tower" (offline + wakeable), then A: the wake starts.
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    s.handle_menu(MenuEvent::Confirm);
    let w = s
        .wake
        .as_ref()
        .expect("Waking card raised in the SAME call as the A press");
    assert_eq!(w.name, "Office Tower");
    assert!(!w.online);
    // The very next input is modal-gated — the cursor can't drift onto Add Host —
    // and sync (which runs first in handle_menu) must not clear the placeholder
    // before the service thread reports its first real status.
    assert!(s.handle_menu(MenuEvent::Move(MenuDir::Right)).is_none());
    assert!(
        s.wake.is_some(),
        "optimistic card survived a sync with no service status"
    );
    // B cancels: the gate releases and navigation works again.
    s.handle_menu(MenuEvent::Back);
    assert!(s.wake.is_none());
    assert!(s.handle_menu(MenuEvent::Move(MenuDir::Left)).is_some());
}

/// Every settings tab actually RASTERS. The eyeball dump below is `#[ignore]`d, so without
/// this nothing in the normal gate ever ran the tab strip's layout arithmetic or a settings
/// screen's rows — a bad index there would only surface on a Deck. CPU raster: the SkSL
/// backdrop, the layers and the text all run without a GPU.
/// Tab / Shift+Tab change section. The strip shipped on the shoulder buttons and
/// PgUp/PgDn only, and the legend names PgUp/PgDn solely when NO pad is attached — so with
/// a controller plugged in a keyboard user had no way in, and no way to find one.
#[test]
fn tab_and_shift_tab_change_section() {
    use sdl3::keyboard::Scancode;
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.handle_menu(MenuEvent::Tertiary); // X → Settings
    s.motion = Motion::None; // skip the push transition, which drops input
    let tab = |s: &Shell| match s.stack.last() {
        Some(Screen::Settings(st)) => st.tab_for_test(),
        _ => panic!("the settings screen is on top"),
    };
    assert_eq!(tab(&s), 0);
    assert!(s.key(Scancode::Tab, false, false), "Tab is consumed");
    assert_eq!(tab(&s), 1, "Tab goes forward");
    assert!(s.key(Scancode::Tab, true, false));
    assert_eq!(tab(&s), 0, "Shift+Tab goes back");
    // …and it wraps backwards off the first tab, exactly as the shoulders do.
    s.key(Scancode::Tab, true, false);
    assert_eq!(tab(&s), crate::screens::settings::TAB_COUNT - 1);
    // A key repeat must not run through the strip a section per frame held.
    let before = tab(&s);
    s.key(Scancode::Tab, false, true);
    assert_eq!(tab(&s), before, "held Tab doesn't skip sections");
}

/// A right-click is Back on every screen, so a pointer always has a way out.
#[test]
fn a_secondary_press_goes_back() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.handle_menu(MenuEvent::Tertiary); // X → Settings
    s.motion = Motion::None;
    assert_eq!(s.stack.len(), 2);
    assert!(s.pointer(crate::pointer::Pointer {
        x: 10.0,
        y: 10.0,
        kind: crate::pointer::PointerKind::Back,
    }));
    // The pop runs through the same transition a B press does.
    assert!(matches!(s.motion, Motion::Pop { .. }));
}

/// Up on a saved tile opens that host's menu; a discovered-but-unsaved one has none.
#[test]
fn up_opens_host_options_for_saved_tiles_only() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.handle_menu(MenuEvent::Move(MenuDir::Up));
    assert!(
        matches!(s.stack.last(), Some(Screen::HostOptions(_))),
        "the first tile is a saved host"
    );
    s.motion = Motion::None;
    s.handle_menu(MenuEvent::Back);
    s.motion = Motion::None;
    // The third fixture host is discovered-only (`saved: false`).
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    s.handle_menu(MenuEvent::Move(MenuDir::Up));
    assert!(
        matches!(s.stack.last(), Some(Screen::Home(_))),
        "an unsaved host has nothing to edit or forget"
    );
}

#[test]
fn every_settings_tab_rasters() {
    let fonts = crate::theme::build_fonts().unwrap();
    let (w, h) = (1280u32, 800u32);
    let pads: Vec<PadInfo> = Vec::new();
    let mut surface = skia_safe::surfaces::raster_n32_premul((w as i32, h as i32)).unwrap();
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.handle_menu(MenuEvent::Tertiary); // X → Settings

    let mut frame = |s: &mut Shell| {
        s.render(
            surface.canvas(),
            w,
            h,
            &fonts,
            Some("Xbox Wireless Controller"),
            Some(GamepadPref::Xbox360),
            &pads,
        );
    };
    // One lap of the strip — R1 wraps back to where it started. Every tab's rows fit on an
    // 800-tall window at once, so ONE frame per tab draws all of them; the cursor is walked to
    // the end first (input only, no render) so the focused and unfocused row paths both run.
    // Deliberately frugal: a full-screen SkSL field on the CPU costs the better part of a second
    // per frame in a debug build, and this test's job is to catch a panic, not to look pretty.
    for _ in 0..crate::screens::settings::TAB_COUNT {
        for _ in 0..12 {
            s.handle_menu(MenuEvent::Move(MenuDir::Down));
        }
        frame(&mut s);
        s.handle_menu(MenuEvent::JumpForward);
    }
    // A narrow window is the case the strip has to shrink for (the pills are laid out from
    // measured text, so a too-small width must clamp rather than lay out off-screen).
    s.render(surface.canvas(), 640, 400, &fonts, None, None, &pads);
}

/// Render every console scene to PNGs for the eyeball pass (ignored; run with
/// `PF_CONSOLE_DUMP=<dir> cargo test -p pf-console-ui --release -- --ignored dump`).
/// CPU raster — the SkSL aurora, layers and text all run without a GPU.
#[test]
#[ignore]
fn dump_console_screens() {
    let dir = std::env::var("PF_CONSOLE_DUMP").expect("set PF_CONSOLE_DUMP to an output dir");
    let fonts = crate::theme::build_fonts().unwrap();
    let (w, h) = (1280, 800);
    let pads: Vec<PadInfo> = Vec::new();
    let dump = |shell: &mut Shell, frames: usize, sleep_ms: u64, name: &str, pad: bool| {
        let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
        for _ in 0..frames {
            shell.render(
                surface.canvas(),
                w as u32,
                h as u32,
                &fonts,
                pad.then_some("Xbox Wireless Controller"),
                pad.then_some(GamepadPref::Xbox360),
                &pads,
            );
            std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
        }
        let png = surface
            .image_snapshot()
            .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
            .unwrap();
        std::fs::write(format!("{dir}/{name}.png"), png.as_bytes()).unwrap();
    };

    // Home, settled, with a pad (Letters glyphs).
    let (mut s, console, library) = shell(vec![Screen::Home(HomeScreen::new())]);
    dump(&mut s, 40, 8, "01-home", true);

    // The host menu — Up on the focused saved tile. The home frame above carries the new
    // ▲ Options hint that leads here, so the two are worth eyeballing together.
    s.handle_menu(MenuEvent::Move(MenuDir::Up));
    dump(&mut s, 40, 8, "01b-host-options", true);
    s.handle_menu(MenuEvent::Back);
    dump(&mut s, 20, 8, "_settle0", true);

    // Mid-push into Settings (the transition still): a couple of fast frames land
    // the capture around p ≈ 0.4 — both layers visible.
    s.handle_menu(MenuEvent::Tertiary);
    dump(&mut s, 3, 25, "02-transition", true);
    dump(&mut s, 40, 8, "03-settings", true);

    // The Interface tab (5 shoulder presses along) leads with the Background row, so these
    // frames show the strip mid-list AND the palette picker. Palettes are set directly rather
    // than by counting Confirm presses, so reordering the table can't silently shoot the wrong
    // one. Each is a whole LOOK, not just a backdrop: accent, ink and scrim move together, so
    // the pale ones must be eyeballed with dark text on them.
    for _ in 0..5 {
        s.handle_menu(MenuEvent::JumpForward);
    }
    for id in ["violet", "oled", "ember", "abyss", "holo", "sunset", "mint"] {
        s.settings.ui_palette = id.to_string();
        dump(&mut s, 40, 8, &format!("03-settings-{id}"), true);
    }
    // Back to the first tab so the later scenes look like they always did.
    for _ in 0..5 {
        s.handle_menu(MenuEvent::JumpBack);
    }
    // …and the LAUNCHER at full contrast under a few of them — the backdrop's loudest form,
    // and the one the palettes are really chosen by.
    s.handle_menu(MenuEvent::Back);
    dump(&mut s, 20, 8, "_settle", true);
    for id in ["nebula", "sunset", "holo"] {
        s.settings.ui_palette = id.to_string();
        dump(&mut s, 40, 8, &format!("01-home-{id}"), true);
    }
    s.settings.ui_palette = "violet".to_string();
    dump(&mut s, 20, 8, "_settle2", true);
    s.handle_menu(MenuEvent::Tertiary); // back into Settings for the scenes below
    dump(&mut s, 20, 8, "_settle3", true);

    // Add Host with the keyboard tray up (keyboard glyph style: no pad).
    s.handle_menu(MenuEvent::Back);
    dump(&mut s, 40, 8, "_back", true);
    for _ in 0..3 {
        s.handle_menu(MenuEvent::Move(MenuDir::Right));
    }
    s.handle_menu(MenuEvent::Confirm); // Add Host screen
    dump(&mut s, 40, 8, "04-addhost", false);
    s.handle_menu(MenuEvent::Confirm); // open the Name keyboard
    for ev in [
        MenuEvent::Move(MenuDir::Down),
        MenuEvent::Confirm,
        MenuEvent::Confirm,
    ] {
        s.handle_menu(ev);
    }
    dump(&mut s, 40, 8, "05-addhost-keyboard", false);

    // Pair (focused on the unpaired discovered host).
    s.handle_menu(MenuEvent::Back); // close keyboard
    s.handle_menu(MenuEvent::Back); // leave add-host
    dump(&mut s, 40, 8, "_back2", true);
    s.handle_menu(MenuEvent::Move(MenuDir::Left)); // onto "steambox"
    s.handle_menu(MenuEvent::Confirm);
    dump(&mut s, 40, 8, "06-pair", true);

    // Library with placeholder posters.
    library.set_games(
        [
            "Hades II",
            "Elden Ring",
            "Hollow Knight",
            "Baldur's Gate 3",
            "Celeste",
            "Deep Rock Galactic",
            "Portal 2",
        ]
        .iter()
        .enumerate()
        .map(|(i, t)| crate::library::LibraryGame {
            id: format!("steam:{i}"),
            title: (*t).to_string(),
            store: "steam".into(),
            launcher: false,
        })
        .collect(),
    );
    let (mut s2, _c2, _l2) = {
        let console2 = ConsoleShared::default();
        console2.set_hosts(hosts());
        let bus = ConsoleBus::default();
        let sh = Shell::new(
            console2.clone(),
            library.clone(),
            bus,
            ConsoleOptions {
                device_name: "deck".into(),
                deck: false,
            },
            vec![
                Screen::Home(HomeScreen::new()),
                Screen::Library(LibraryScreen::new(&hosts()[0])),
            ],
        )
        .unwrap();
        (sh, console2, library.clone())
    };
    s2.handle_menu(MenuEvent::Move(MenuDir::Right));
    s2.handle_menu(MenuEvent::Move(MenuDir::Right));
    dump(&mut s2, 40, 8, "07-library", true);

    // The wake and connecting overlays + a toast.
    console.set_wake(Some(WakeStatus {
        key: "bb22".into(),
        name: "Office Tower".into(),
        seconds: 12,
        timed_out: false,
        online: false,
        then_connect: true,
    }));
    dump(&mut s, 10, 8, "08-waking", true);
    console.set_wake(Some(WakeStatus {
        key: "bb22".into(),
        name: "Office Tower".into(),
        seconds: 90,
        timed_out: true,
        online: false,
        then_connect: true,
    }));
    dump(&mut s, 10, 8, "08b-wake-timed-out", true);
    console.set_wake(None);
    s.set_connecting(Some("Elden Ring".into()));
    dump(&mut s, 10, 8, "09-connecting", true);
    s.set_connecting(None);
    s.session_failed("Connection timed out");
    dump(&mut s, 10, 8, "10-toast", true);
}
