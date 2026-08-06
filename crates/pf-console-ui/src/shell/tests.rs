use super::*;
use crate::model::WakeStatus;
use crate::screens::home::HomeScreen;
use crate::screens::library::LibraryScreen;
use punktfunk_core::config::GamepadPref;

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

    // Mid-push into Settings (the transition still): a couple of fast frames land
    // the capture around p ≈ 0.4 — both layers visible.
    s.handle_menu(MenuEvent::Tertiary);
    dump(&mut s, 3, 25, "02-transition", true);
    dump(&mut s, 40, 8, "03-settings", true);

    // The Interface tab (5 shoulder presses along) leads with the Background row, so this frame
    // shows both the strip mid-list and the palette picker…
    for _ in 0..5 {
        s.handle_menu(MenuEvent::JumpForward);
    }
    dump(&mut s, 40, 8, "03b-settings-interface", true);
    // …and cycling it three times lands on Ember, which is the whole point: the CALM backdrop
    // behind these rows recolours live.
    for _ in 0..3 {
        s.handle_menu(MenuEvent::Confirm);
    }
    dump(&mut s, 40, 8, "03c-settings-ember", true);
    // Back to the brand default and the first tab so the later scenes look like they always did.
    for _ in 0..3 {
        s.handle_menu(MenuEvent::Confirm);
    }
    for _ in 0..5 {
        s.handle_menu(MenuEvent::JumpBack);
    }

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
