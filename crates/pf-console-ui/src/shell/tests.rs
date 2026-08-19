use super::*;
use crate::model::WakeStatus;
use crate::screens::home::HomeScreen;
use crate::screens::library::LibraryScreen;
use punktfunk_core::config::GamepadPref;

/// The screen-transition contract, against the shared vectors. Every client re-implements this
/// motion in its own animation system, so the numbers exist in three places and drifted in two of
/// them before there was a test.
///
/// This side reads `motion_spring` (vectors version 2). The v1 `motion` block is still in the
/// file and still correct — the Android client's `ConsoleVectorsTest` pins it, and the Apple
/// client's `GamepadShell` mirrors its constants — but the desktop console's transition is a
/// damped spring now rather than a 0.26 s ease-out-cubic, so it no longer implements that block
/// and says so here rather than quietly passing a test about a curve it does not run.
///
/// Springs are INTEGRATOR-dependent, which is why v2 pins parameters where v1 pinned sampled
/// positions: two runtimes that both honour `response`/`damping` agree to the eye and disagree
/// in the third decimal, and sampling would pin the disagreement instead of the feel.
#[test]
fn motion_matches_the_shared_vectors() {
    let raw = include_str!("../../../../clients/shared/console-vectors.json");
    let file: serde_json::Value =
        serde_json::from_str(raw).expect("console-vectors.json must parse");
    assert_eq!(
        file["version"].as_u64(),
        Some(2),
        "the spring block arrived with version 2"
    );

    let m = &file["motion_spring"];
    let num = |key: &str| m[key].as_f64().unwrap_or_else(|| panic!("{key} missing"));
    let close = |what: &str, got: f64, want: f64| {
        assert!(
            (got - want).abs() < 1e-9,
            "{what} is {got}, vectors say {want}"
        );
    };
    close(
        "response",
        crate::anim::springs::NAV.response,
        num("response"),
    );
    close("damping", crate::anim::springs::NAV.damping, num("damping"));
    close("push slide", NAV_SLIDE_DP, num("push_slide_dp"));
    close("enter scale", NAV_ENTER_SCALE, num("enter_scale"));
    close("exit scale", NAV_EXIT_SCALE, num("exit_scale"));
    close("reveal alpha", NAV_REVEAL_ALPHA, num("reveal_alpha"));
    assert_eq!(
        m["interruptible"].as_bool(),
        Some(true),
        "this client's transitions accept Back mid-flight; the block must say so"
    );

    // The v1 block stays put until the last client migrates, and stays MARKED so nobody
    // reads it as live. Deleting it here would silently red Android's test instead.
    assert!(
        file["motion"]["$deprecated"].is_string(),
        "the v1 motion block must carry its deprecation note while other clients read it"
    );
}

/// Point the settings/known-hosts stores at a throwaway HOME — the settings screen
/// SAVES on adjust, and a test must never write the developer's real config.
fn fake_home() {
    use std::sync::OnceLock;
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("pf-console-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: runs at most once, inside `get_or_init` — concurrent `fake_home` callers
        // block until it returns, and nothing else in this binary mutates `HOME`. (The old
        // re-set after the closure ran on EVERY call, so two parallel tests could race the
        // write; setting once under the OnceLock is what makes this sound.)
        unsafe { std::env::set_var("HOME", &dir) };
        dir
    });
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
        clipboard_sync: false,
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
        ConsoleOptions::desktop("deck".into(), false),
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
    // While connecting: B cancels — and the takeover comes down on the spot. It must NOT wait
    // for a session phase to clear it: the dial is blocking on the host's side of this
    // interface, so that wait was the whole connect budget, and an embedder that just drops a
    // canceled dial sends no phase at all — the console stuck on "Canceling…" until the app died.
    s.handle_menu(MenuEvent::Back);
    assert!(matches!(
        s.take_action(),
        Some(OverlayAction::CancelConnect)
    ));
    assert!(s.connecting.is_none(), "cancel drops the takeover itself");
    // A dial that resolves afterwards (or never) changes nothing.
    s.session_ended(None);
    assert!(s.connecting.is_none());
}

fn finish_motion(s: &mut Shell) {
    // Tests fast-forward transitions. Seats the spring on its target and runs the REAL
    // settle, rather than wishing the motion away — otherwise a reversed push would skip
    // the bookkeeping that takes its screen back off the stack.
    if let Motion::Nav { spring, target, .. } = &mut s.motion {
        spring.pos = *target;
        spring.vel = 0.0;
    }
    s.finish_nav();
}

/// Step the transition at a fixed `dt` until it settles, collecting every position it
/// passed through. Bounded so a spring that never settles fails the test instead of
/// hanging it.
fn run_motion(s: &mut Shell) -> Vec<f64> {
    let mut path = Vec::new();
    for _ in 0..600 {
        match s.advance_nav(1.0 / 120.0) {
            Some(p) => path.push(p),
            None => return path,
        }
    }
    panic!("transition never settled");
}

/// A pinned host+profile card's library launches with THAT profile (design §5.2a).
///
/// The card's plain A-press always carried its profile; Y — which the card offers, being
/// paired and saved — opened a library screen that knew only the host, so every title
/// launched off it silently fell back to the host's default binding. The profile a user
/// pinned is the whole reason they pressed that card.
#[test]
fn a_pinned_cards_library_launches_with_its_profile() {
    let mut rows = hosts();
    let card = HostRow {
        key: "aa11\u{0}hdr".into(),
        pin: Some(crate::model::ProfileChip {
            id: "hdr".into(),
            name: "HDR".into(),
            accent: None,
        }),
        ..rows[0].clone()
    };
    rows.insert(1, card);
    let (mut s, console, library) = shell(vec![Screen::Home(HomeScreen::new())]);
    console.set_hosts(rows);
    s.sync();

    // Focus the pinned card (it sits right after its host's primary tile), then Y.
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    s.handle_menu(MenuEvent::Secondary);
    finish_motion(&mut s);
    match s.stack.last() {
        Some(Screen::Library(l)) => assert_eq!(
            l.title(),
            "Living Room PC \u{b7} HDR",
            "the shelf names the profile it will launch with"
        ),
        _ => panic!("Y on a pinned card opens its library"),
    }

    library.set_games(vec![crate::library::LibraryGame {
        id: "steam:570".into(),
        title: "Dota 2".into(),
        store: "steam".into(),
        launcher: false,
        icon: String::new(),
        platform: None,
        running: false,
    }]);
    s.handle_menu(MenuEvent::Confirm);
    match s.take_action() {
        Some(OverlayAction::Launch {
            launch, profile, ..
        }) => {
            assert_eq!(launch.as_deref(), Some("steam:570"));
            assert_eq!(
                profile.as_deref(),
                Some("hdr"),
                "the launch carries the pinned card's profile"
            );
        }
        _ => panic!("A on a title raises a launch"),
    }
}

/// …and off the host's PRIMARY tile there is no one-off: the host's binding decides,
/// which is what the resolver sees as `None`.
#[test]
fn a_primary_tiles_library_leaves_the_profile_to_the_binding() {
    let (mut s, _console, library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.handle_menu(MenuEvent::Secondary); // paired+online host focused first
    finish_motion(&mut s);
    library.set_games(vec![crate::library::LibraryGame {
        id: "steam:570".into(),
        title: "Dota 2".into(),
        store: "steam".into(),
        launcher: false,
        icon: String::new(),
        platform: None,
        running: false,
    }]);
    s.handle_menu(MenuEvent::Confirm);
    assert!(matches!(
        s.take_action(),
        Some(OverlayAction::Launch { profile: None, .. })
    ));
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
    use crate::input::Key as Scancode;
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
    assert!(matches!(
        s.motion,
        Motion::Nav {
            kind: NavKind::Pop,
            ..
        }
    ));
}

/// A REPLACE recedes the screen it replaced, not that screen's parent.
///
/// Reported from a Deck: choosing "Edit…" in a host's menu flashed the host LIST for the
/// length of the transition before the editor arrived. The cause is that a push paints the
/// screen beneath the incoming one as its receding layer, while a replace had already popped
/// and dropped the screen being swapped out — so "beneath" was the menu's parent, one level
/// too far, and the transition animated the editor in over Home.
///
/// Asserted on the carried screen rather than on pixels: the defect is entirely a question of
/// WHICH screen the motion holds, and a frame diff would pin the particular look of a
/// transition instead of the thing that was wrong with it.
#[test]
fn a_replace_carries_the_screen_it_replaced() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.handle_menu(MenuEvent::Move(MenuDir::Up));
    assert!(matches!(s.stack.last(), Some(Screen::HostOptions(_))));
    finish_motion(&mut s);

    // Walk to "Edit…" and take it. The first fixture host is paired and online and cannot
    // wake, so its menu is [Send logs, Copy link, Edit…, Forget, Cancel] — Edit is two down.
    // Pressed exactly rather than searched, so that reordering the menu fails HERE instead of
    // quietly landing this test's Confirm on "Forget".
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    s.handle_menu(MenuEvent::Confirm);
    assert!(
        matches!(s.stack.last(), Some(Screen::AddHost(_))),
        "Edit… opens the host editor"
    );
    assert_eq!(s.stack.len(), 2, "the menu was swapped out, not stacked on");
    match &s.motion {
        Motion::Nav {
            kind: NavKind::Push,
            leaving: Some(carried),
            ..
        } => assert!(
            matches!(carried.as_ref(), Screen::HostOptions(_)),
            "the receding layer must be the MENU; carrying nothing leaves the renderer to \
             recede the menu's parent, which is the reported flash"
        ),
        _ => panic!("a replace must be a push CARRYING its predecessor"),
    }

    // …and reversing it puts the menu back, because that is the screen the user watched
    // recede and then return.
    s.handle_menu(MenuEvent::Back);
    finish_motion(&mut s);
    assert!(
        matches!(s.stack.last(), Some(Screen::HostOptions(_))),
        "a reversed replace lands where the user actually was"
    );
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

/// The settings screen with one frame rendered, so its rows have real rects to press.
fn rendered_settings() -> (Shell, skia_safe::Rect) {
    let fonts = crate::theme::build_fonts().unwrap();
    let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.handle_menu(MenuEvent::Tertiary); // X → Settings
    finish_motion(&mut s);
    s.render(surface.canvas(), 1280, 800, &fonts, None, None, &[]);
    let row = match s.stack.last() {
        Some(Screen::Settings(scr)) => scr.row_rect_for_test(0).expect("the list drew its rows"),
        _ => panic!("settings is not on top"),
    };
    (s, row)
}

/// The whole point of the touch tracker: a finger swiping across the settings list is
/// SCROLLING, and must not flip the value it happened to land on — which is exactly what
/// the press-acts-on-contact model did to every swipe before the `touch` flag existed.
/// The same contact lifted in place IS the tap, delivered on the lift at the anchor.
#[test]
fn a_touch_swipe_scrolls_settings_without_changing_a_value() {
    use pf_client_core::console::{PointerButton, PointerInput};
    let (mut s, row) = rendered_settings();
    let (cx, cy) = (row.center_x(), row.center_y());
    // The Resolution row's whole observable state: activating it steps the D1 tri-state
    // Native -> Match window, which flips the FLAG while width/height stay (0, 0).
    let state = |s: &Shell| (s.settings.match_window, s.settings.width, s.settings.height);
    let before = state(&s);

    // Finger lands on the Resolution row and swipes up, well past slop and several ticks.
    s.pointer_input(PointerInput::Down {
        x: cx,
        y: cy,
        button: PointerButton::Primary,
        touch: true,
    });
    for i in 1..=6 {
        s.pointer_input(PointerInput::Move {
            x: cx,
            y: cy - (i as f32) * 40.0,
        });
    }
    s.pointer_input(PointerInput::Up {
        x: cx,
        y: cy - 240.0,
        button: PointerButton::Primary,
    });
    assert_eq!(
        state(&s),
        before,
        "a swipe across a row is a scroll, not a value change"
    );

    // The same contact, lifted where it landed: a tap. Deferred — nothing on contact,
    // the step on the lift.
    s.pointer_input(PointerInput::Down {
        x: cx,
        y: cy,
        button: PointerButton::Primary,
        touch: true,
    });
    assert_eq!(state(&s), before, "a touch press must not act on contact");
    s.pointer_input(PointerInput::Up {
        x: cx,
        y: cy,
        button: PointerButton::Primary,
    });
    assert_ne!(
        state(&s),
        before,
        "the tap lands on the lift, at the anchor"
    );
}

/// A mouse is not a finger: its press keeps acting on contact, exactly as before the
/// touch flag existed.
#[test]
fn a_mouse_press_still_acts_on_contact() {
    use pf_client_core::console::{PointerButton, PointerInput};
    let (mut s, row) = rendered_settings();
    let state = |s: &Shell| (s.settings.match_window, s.settings.width, s.settings.height);
    let before = state(&s);
    s.pointer_input(PointerInput::Down {
        x: row.center_x(),
        y: row.center_y(),
        button: PointerButton::Primary,
        touch: false,
    });
    assert_ne!(state(&s), before, "a mouse click acts on the press");
}

/// A horizontal drag on Home steps the carousel — one tick per `DRAG_TICK_DP` of travel
/// past the slop — and the lift after a drag presses nothing. Needs no render: ticks act
/// on the cursor, not on drawn rects.
#[test]
fn a_horizontal_drag_steps_the_home_carousel() {
    use pf_client_core::console::{PointerButton, PointerInput};
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.pointer_input(PointerInput::Down {
        x: 640.0,
        y: 400.0,
        button: PointerButton::Primary,
        touch: true,
    });
    // First move leaves the slop (locks the horizontal axis); the second travels one full
    // tick leftward — content follows the finger, so the NEXT tile comes up.
    s.pointer_input(PointerInput::Move { x: 620.0, y: 400.0 });
    s.pointer_input(PointerInput::Move {
        x: 620.0 - DRAG_TICK_DP as f32,
        y: 400.0,
    });
    s.pointer_input(PointerInput::Up {
        x: 620.0 - DRAG_TICK_DP as f32,
        y: 400.0,
        button: PointerButton::Primary,
    });
    // The fixture's second host (Office Tower) is offline with a stored MAC: Confirm on it
    // raises the wake card. That proves the drag moved the cursor — and that the lift
    // after a drag pressed nothing (a press would have acted before Confirm ran).
    assert!(
        s.wake.is_none(),
        "the drag itself must not activate anything"
    );
    s.handle_menu(MenuEvent::Confirm);
    assert!(
        s.wake.is_some(),
        "Confirm after a one-tick drag lands on the second host's wake"
    );
}

/// A canceled touch (the finger left the window, the toolkit stole the gesture) is
/// dropped whole: no press ever lands.
#[test]
fn a_canceled_touch_never_acts() {
    use pf_client_core::console::{PointerButton, PointerInput};
    let (mut s, row) = rendered_settings();
    let state = |s: &Shell| (s.settings.match_window, s.settings.width, s.settings.height);
    let before = state(&s);
    s.pointer_input(PointerInput::Down {
        x: row.center_x(),
        y: row.center_y(),
        button: PointerButton::Primary,
        touch: true,
    });
    s.pointer_input(PointerInput::Cancel);
    s.pointer_input(PointerInput::Up {
        x: row.center_x(),
        y: row.center_y(),
        button: PointerButton::Primary,
    });
    assert_eq!(
        state(&s),
        before,
        "cancel dropped the gesture; the stray lift presses nothing"
    );
}

/// The work package's whole reason for existing: Back pressed mid-push is HEARD, and it
/// turns the screen around rather than queuing a second animation behind the first.
///
/// The continuity assertion is the important half. A naive "cancel and play a pop" reads
/// as a snap because the two recipes disagree about where things are; retargeting the same
/// spring cannot snap, because position is carried and only the target moved. This asserts
/// the position never jumps by more than a frame's worth of the travel it was already
/// doing.
#[test]
fn back_mid_push_turns_the_screen_around() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.handle_menu(MenuEvent::Tertiary); // X → Settings
    assert_eq!(s.stack.len(), 2);

    // Let it get properly under way, then interrupt.
    let mut before = 0.0;
    for _ in 0..12 {
        before = s.advance_nav(1.0 / 120.0).expect("still in flight");
    }
    assert!(before > 0.05 && before < 0.95, "mid-flight, got {before}");
    assert!(s.nav_back(), "Back is answered by the transition itself");
    assert_eq!(
        s.stack.len(),
        2,
        "the screen is still on the stack while it flies back"
    );

    let path = run_motion(&mut s);
    assert!(!path.is_empty(), "the reversal actually animated");
    // No snap: the first sample after the retarget continues from where it was.
    assert!(
        (path[0] - before).abs() < 0.05,
        "jumped from {before} to {}",
        path[0]
    );
    // And it goes DOWN — the screen is leaving, having briefly been arriving.
    assert!(*path.last().expect("non-empty") < before);
    assert_eq!(
        s.stack.len(),
        1,
        "the reversed push took its screen back off"
    );
    assert!(matches!(s.motion, Motion::None));
}

/// Back at the ROOT is not a reversal — there is no parent to fall back to, and B there
/// means quit. The transition declines it so the normal path can answer.
#[test]
fn back_mid_push_at_the_root_is_left_to_the_normal_path() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    // A Replace at the root pushes without deepening the stack.
    s.apply_nav(crate::screens::Nav::Replace(Box::new(Screen::Home(
        HomeScreen::new(),
    ))));
    assert_eq!(s.stack.len(), 1);
    s.advance_nav(1.0 / 120.0);
    assert!(!s.nav_back(), "nothing to reverse into");
    finish_motion(&mut s);
    assert_eq!(s.stack.len(), 1, "and the root survived");
}

/// A mid-pop A is refused: activating a half-dismissed screen is a mis-tap, not intent.
/// A mid-pop BACK, on the other hand, is exactly what a held B is — it starts the next pop
/// at once, which is the stutter this work package removes.
#[test]
fn mid_pop_refuses_confirm_but_honours_another_back() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.handle_menu(MenuEvent::Tertiary); // → Settings
    finish_motion(&mut s);
    s.handle_menu(MenuEvent::Move(MenuDir::Down)); // a row that would push if activated
    s.handle_menu(MenuEvent::Back); // start the pop
    assert!(matches!(s.motion, Motion::Nav { .. }));
    let mid = s.advance_nav(1.0 / 120.0).expect("in flight");
    assert!(mid < NAV_INPUT_OPENS, "the test needs an early sample");

    let depth = s.stack.len();
    assert!(
        s.handle_menu(MenuEvent::Confirm).is_none(),
        "A mid-pop does nothing"
    );
    assert_eq!(s.stack.len(), depth, "and pushes nothing");

    // Back again, though, walks out another level — here that is the root, so it quits.
    s.handle_menu(MenuEvent::Back);
    finish_motion(&mut s);
    assert!(matches!(s.take_action(), Some(OverlayAction::Quit)));
}

/// A completed pop frees the screen it was carrying, and hint rects are published only
/// once the shell is settled — the invariant that predates springs and survives them,
/// because "settled" is still exactly `Motion::None`.
#[test]
fn a_completed_pop_frees_its_screen_and_republishes_hints() {
    let fonts = crate::theme::build_fonts().unwrap();
    let pads: Vec<PadInfo> = Vec::new();
    let (w, h) = (640u32, 400u32);
    let mut surface = skia_safe::surfaces::raster_n32_premul((w as i32, h as i32)).unwrap();
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.handle_menu(MenuEvent::Tertiary);
    finish_motion(&mut s);
    s.handle_menu(MenuEvent::Back);

    // Mid-pop: a screen is being carried, and the legend is not clickable.
    assert!(
        matches!(
            &s.motion,
            Motion::Nav {
                leaving: Some(_),
                ..
            }
        ),
        "the popped screen is parked on the motion"
    );
    s.render(surface.canvas(), w, h, &fonts, None, None, &pads);
    assert!(
        s.hint_rects.is_empty(),
        "mid-transition the drawn rects are slid and scaled, so none are published"
    );

    run_motion(&mut s);
    assert!(matches!(s.motion, Motion::None));
    s.render(surface.canvas(), w, h, &fonts, None, None, &pads);
    assert!(
        !s.hint_rects.is_empty(),
        "settled, the legend is clickable again"
    );
}

/// Draw one frame at a small size. A freshly pushed screen has not seen the shared model
/// yet — it adopts it on its first sync — so a test that asserts on a screen's CONTENT
/// straight after pushing it is asking before the answer exists. The app always renders;
/// so does this.
fn frame(s: &mut Shell) {
    let fonts = crate::theme::build_fonts().unwrap();
    let pads: Vec<PadInfo> = Vec::new();
    let mut surface = skia_safe::surfaces::raster_n32_premul((480, 300)).unwrap();
    s.render(surface.canvas(), 480, 300, &fonts, None, None, &pads);
}

/// A library with more than one group, for the collections flow.
fn mixed_library(library: &LibraryShared) {
    let g = |id: &str, title: &str, store: &str, platform: Option<&str>, launcher: bool| {
        crate::library::LibraryGame {
            id: id.into(),
            title: title.into(),
            store: store.into(),
            launcher,
            icon: String::new(),
            platform: platform.map(str::to_string),
            running: false,
        }
    };
    library.set_games(vec![
        g("l1", "Steam Big Picture", "steam", None, true),
        g("s1", "Dota 2", "steam", None, false),
        g("s2", "Half-Life", "steam", None, false),
        g("p1", "Demon's Souls", "custom", Some("PS3"), false),
        g("p2", "The Last of Us", "custom", Some("PS3"), false),
        g("n1", "Super Metroid", "custom", Some("SNES"), false),
    ]);
}

/// "Start in collections" actually starts in collections — asserted on the SHELL, because
/// the shelf was never the part that was broken.
///
/// The handover shipped dead: `LibraryScreen::collections_upgrade` was written, documented and
/// unit-tested for its DECISION, and then nothing ever called it. It carried an
/// `#[allow(dead_code)]`, which is precisely what stopped the compiler from saying so, and the
/// shelf's own tests passed throughout because they called it directly. The setting was on,
/// the shelf agreed it should stand aside, and the library opened on the shelf anyway.
///
/// So this drives `Shell::sync` and asserts on the STACK. A screen cannot replace itself —
/// only the shell owns the stack — so the shell is where the wiring has to be witnessed.
#[test]
fn the_setting_hands_a_multi_platform_library_over_to_collections() {
    let games: Vec<crate::library::LibraryGame> = platform_games();
    for (want_collections, enabled) in [(true, true), (false, false)] {
        let (mut s, _console, library) = shell(vec![
            Screen::Home(HomeScreen::new()),
            Screen::Library(LibraryScreen::new(&hosts()[0], 0)),
        ]);
        s.settings.library_collections = enabled;

        // The shelf reads the model only once its OWN fetch has begun: a library that is Ready
        // before that is the PREVIOUS host's, still sitting in the shared model. `begin_fetch`
        // is what the service thread does when it drains the queued `FetchLibrary`, and the
        // epoch it raises is what the shelf compares against the one it was pushed at.
        s.sync();
        assert!(
            matches!(s.stack.last(), Some(Screen::Library(_))),
            "nothing to hand over to while the fetch is still out"
        );

        library.begin_fetch();
        library.set_games(games.clone());
        s.sync();

        if want_collections {
            assert!(
                matches!(s.stack.last(), Some(Screen::Collections(_))),
                "the setting is on and the library has four platforms — it must open on them"
            );
            assert_eq!(
                s.stack.len(),
                2,
                "it REPLACES the shelf, never stacks on it"
            );
        } else {
            assert!(
                matches!(s.stack.last(), Some(Screen::Library(_))),
                "with the setting off the library opens on its shelf"
            );
        }
    }
}

/// …and a library with only ONE collection opens on its shelf whatever the setting says,
/// because a collections screen listing a single tile is a press that buys nothing.
#[test]
fn one_collection_is_not_worth_a_screen() {
    let (mut s, _console, library) = shell(vec![
        Screen::Home(HomeScreen::new()),
        Screen::Library(LibraryScreen::new(&hosts()[0], 0)),
    ]);
    s.settings.library_collections = true;
    s.sync();
    library.begin_fetch();
    library.set_games(
        platform_games()
            .into_iter()
            .map(|mut g| {
                g.platform = Some("PlayStation 3".into());
                g
            })
            .collect(),
    );
    s.sync();
    assert!(
        matches!(s.stack.last(), Some(Screen::Library(_))),
        "one platform is not a set of collections"
    );
}

/// The user's flow, verbatim: group by platform, walk the platforms, pick PS3, see its
/// games — and get back out again. This is the whole point of Part C, so it is asserted
/// end to end rather than in pieces.
#[test]
fn collections_drill_in_reaches_one_platform_and_backs_out() {
    let (mut s, _console, library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    mixed_library(&library);
    s.handle_menu(MenuEvent::Secondary); // Y at home → this host's library
    finish_motion(&mut s);
    assert!(matches!(s.stack.last(), Some(Screen::Library(_))));

    // Y again → Collections.
    s.handle_menu(MenuEvent::Secondary);
    finish_motion(&mut s);
    assert!(
        matches!(s.stack.last(), Some(Screen::Collections(_))),
        "Y on a multi-group library opens the collections"
    );

    // Walk to the PS3 tile. Groups sort A–Z with launchers pinned first, so the strip
    // reads: Launchers, PS3, SNES, Steam.
    s.handle_menu(MenuEvent::Move(MenuDir::Right));

    // A opens that collection as a filtered shelf.
    s.handle_menu(MenuEvent::Confirm);
    finish_motion(&mut s);
    frame(&mut s); // the new shelf adopts the shared model on its first sync
    let Some(Screen::Library(shelf)) = s.stack.last() else {
        panic!("A on a collection tile opens a shelf");
    };
    assert_eq!(shelf.len_for_test(), 2, "PS3 has exactly its two titles");
    assert!(
        shelf.title().ends_with("PS3"),
        "the breadcrumb names the collection: {}",
        shelf.title()
    );

    // B B walks back out to the unfiltered shelf.
    s.handle_menu(MenuEvent::Back);
    finish_motion(&mut s);
    assert!(matches!(s.stack.last(), Some(Screen::Collections(_))));
    s.handle_menu(MenuEvent::Back);
    finish_motion(&mut s);
    frame(&mut s);
    let Some(Screen::Library(shelf)) = s.stack.last() else {
        panic!("back to the library");
    };
    assert_eq!(shelf.len_for_test(), 6, "the whole library again");
}

/// The gate: a library with nothing to collect must not offer the button, and must not
/// answer it either — a hint and its press have to agree.
#[test]
fn collections_is_offered_only_when_there_is_something_to_browse() {
    let (mut s, _console, library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    // One store, no platforms: a single group.
    library.set_games(vec![
        crate::library::LibraryGame {
            id: "a".into(),
            title: "Dota 2".into(),
            store: "steam".into(),
            launcher: false,
            icon: String::new(),
            platform: None,
            running: false,
        },
        crate::library::LibraryGame {
            id: "b".into(),
            title: "Half-Life".into(),
            store: "steam".into(),
            launcher: false,
            icon: String::new(),
            platform: None,
            running: false,
        },
    ]);
    s.handle_menu(MenuEvent::Secondary);
    finish_motion(&mut s);
    assert!(matches!(s.stack.last(), Some(Screen::Library(_))));
    let depth = s.stack.len();
    assert!(matches!(
        s.handle_menu(MenuEvent::Secondary),
        Some(MenuPulse::Boundary)
    ));
    assert_eq!(s.stack.len(), depth, "and pushed nothing");

    // Now give it a second group and the same press works.
    mixed_library(&library);
    s.handle_menu(MenuEvent::Secondary);
    finish_motion(&mut s);
    assert!(matches!(s.stack.last(), Some(Screen::Collections(_))));
}

/// The trailing Rescan tile asks discovery to look again — and nothing else. It sits one
/// step past Add Host, where a mis-timed press used to land on nothing at all, so the test
/// that matters is that it CANNOT connect: an accidental A on the end of the strip must
/// never start a session.
#[test]
fn the_rescan_tile_probes_and_never_connects() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    // Walk to the very end of the strip: hosts, then Add Host, then Rescan.
    for _ in 0..12 {
        s.handle_menu(MenuEvent::Move(MenuDir::Right));
    }
    assert!(
        s.stack
            .last()
            .is_some_and(|sc| matches!(sc, Screen::Home(_))),
        "still on the home carousel"
    );
    s.handle_menu(MenuEvent::Confirm);

    assert!(
        s.take_action().is_none(),
        "a scan must raise no Launch, and no Quit"
    );
    assert!(s.connecting.is_none(), "and must not open the connect card");
    assert_eq!(s.stack.len(), 1, "and must push no screen");
    assert!(s.toast.is_some(), "it says it is scanning");
    // One step back is Add Host, which DOES push — proof the walk reached the end rather
    // than stalling somewhere harmless.
    s.handle_menu(MenuEvent::Move(MenuDir::Left));
    s.handle_menu(MenuEvent::Confirm);
    assert!(
        matches!(s.stack.last(), Some(Screen::AddHost(_))),
        "the tile before Rescan is Add Host"
    );
}

/// The three toast kinds must be tellable apart WITHOUT reading the words — that is the
/// whole reason the kind exists. In particular the error tint is fixed rather than
/// palette-derived: `moss`'s accent is a green and `ember`'s is an orange, and reporting a
/// failure in the colour the rest of the UI uses for "this is fine" is exactly the bug.
#[test]
fn toast_kinds_are_visually_distinct() {
    use crate::shell::{ToastKind, ToastMark};
    let (info_c, info_m) = ToastKind::Info.look();
    let (ok_c, ok_m) = ToastKind::Success.look();
    let (err_c, err_m) = ToastKind::Error.look();
    assert_eq!(info_m, ToastMark::Dot);
    assert_eq!(ok_m, ToastMark::Check);
    assert_eq!(err_m, ToastMark::Bang);
    let rgb = |c: skia_safe::Color4f| (c.r, c.g, c.b);
    assert_ne!(rgb(info_c), rgb(ok_c));
    assert_ne!(rgb(ok_c), rgb(err_c));

    // Swap in a green-accented palette: Success follows it, Error must not.
    crate::theme::set_ink(crate::theme::Ink::of(crate::library::palette("moss")));
    let (ok_moss, _) = ToastKind::Success.look();
    let (err_moss, _) = ToastKind::Error.look();
    assert_ne!(
        rgb(ok_moss),
        rgb(ok_c),
        "Success takes the palette's accent, so it moved"
    );
    assert_eq!(
        rgb(err_moss),
        rgb(err_c),
        "Error is fixed and must NOT follow the palette"
    );
}

/// Reduced motion: the setting round-trips through the store, the backdrop shader's clock
/// freezes, and the transition shortens. The clock is asserted through `field_clock`
/// rather than by diffing pixels because that IS the decision — `draw_aurora` has exactly
/// one place it reads time, and both callers (the screens and the connect takeover) go
/// through it.
#[test]
fn reduce_motion_freezes_the_field_and_shortens_the_transition() {
    let fonts = crate::theme::build_fonts().unwrap();
    let pads: Vec<PadInfo> = Vec::new();
    let (w, h) = (1280u32, 800u32);
    let mut surface = skia_safe::surfaces::raster_n32_premul((w as i32, h as i32)).unwrap();
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);

    assert!(!s.settings.reduce_motion, "off by default");
    assert_eq!(s.field_clock(12.5), 12.5);
    assert_eq!(s.nav_spec().damping, crate::anim::springs::NAV.damping);

    s.settings.reduce_motion = true;
    assert_eq!(s.field_clock(12.5), 0.0, "the field stops drifting");
    let spec = s.nav_spec();
    assert_eq!(
        spec.damping, 1.0,
        "critically damped: it arrives, never bounces"
    );
    assert!(
        spec.response < crate::anim::springs::NAV.response,
        "and quicker"
    );
    // …and a frame still draws (the shader runs at t = 0 like any other phase).
    s.render(surface.canvas(), w, h, &fonts, None, None, &pads);

    // Round-trip through the settings file, which is what makes it survive a restart.
    s.settings.save();
    let back = pf_client_core::trust::Settings::load();
    assert!(back.reduce_motion, "persisted");
    s.settings.reduce_motion = false;
    s.settings.save();
    assert!(
        !pf_client_core::trust::Settings::load().reduce_motion,
        "and back off again"
    );
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
        // Deterministic time: each frame is one fixed step (the sleep it stands in for, plus
        // the ~4 ms a raster frame costs), so the dump does not depend on the machine's speed
        // or load — the whole point of comparing two of them.
        let step = sleep_ms as f64 / 1000.0 + 0.004;
        shell.fake_clock = Some((shell.fake_clock.map_or(0.0, |(t, _)| t), step));
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
            icon: String::new(),
            platform: None,
            running: false,
        })
        .collect(),
    );
    // A shell parked on this host's shelf. A closure rather than one inline block because
    // there are three scenes over it now — the coverflow, and the sort/view bar at both
    // palette poles — and each needs its own shell, since the entrance and the bar's focus
    // are per-shell state that cannot be rewound.
    let shelf_shell = || {
        let console2 = ConsoleShared::default();
        console2.set_hosts(hosts());
        Shell::new(
            console2,
            library.clone(),
            ConsoleBus::default(),
            ConsoleOptions::desktop("deck".into(), false),
            vec![
                Screen::Home(HomeScreen::new()),
                Screen::Library(LibraryScreen::new(&hosts()[0], 0)),
            ],
        )
        .unwrap()
    };
    let mut s2 = shelf_shell();
    s2.handle_menu(MenuEvent::Move(MenuDir::Right));
    s2.handle_menu(MenuEvent::Move(MenuDir::Right));
    // 80 frames, not 40: this shelf carries no art, so it takes the entrance's 400 ms
    // art-wait deadline, and until that expires the screen is deliberately the loading
    // spinner. At 40×8 ms the dump could finish inside the wait and shoot the SPINNER while
    // claiming to be the coverflow — a screenshot that lies is worse than a missing one.
    dump(&mut s2, 80, 8, "07-library", true);

    // …and the same shelf with the SORT/VIEW bar focused, which is the only state that draws
    // the bar's accent wash. There was no shot of it, which is how the wash shipped covering
    // the whole 46 dp band while its content occupies the top 34 — 2 dp of padding above the
    // pills and 14 below, a backdrop its own content visibly sat high inside. Shot at BOTH
    // palette poles because the wash is `accent(0.14)`: a translucent accent reads differently
    // over a dark field than over a pale one, and this crate has been bitten by exactly that.
    for (name, palette) in [
        ("07c-library-bar", "violet"),
        ("07c-library-bar-mint", "mint"),
    ] {
        let mut s4 = shelf_shell();
        s4.settings.ui_palette = palette.to_string();
        // Settle the shelf first (same 400 ms art-wait as above), THEN Up to the bar: pressing
        // before the field exists would be swallowed and the bar would never take focus.
        dump(&mut s4, 80, 8, "_07c-settle", true);
        s4.handle_menu(MenuEvent::Move(MenuDir::Up));
        dump(&mut s4, 20, 8, name, true);
    }

    // Collections, the drill-in, on a library that actually has PLATFORMS — the scene above
    // has none, so collating it would yield one group and witness nothing.
    //
    // The order below is load-bearing, and the reason there was no collections scene until a
    // tile redesign needed one. `adopt_art` is a ONE-SHOT snapshot taken the moment Y is
    // pressed, so art has to be pushed AND the shelf given frames to decode it BEFORE the
    // press. Press first and every tile renders its monogram, and a deck of covers looks
    // exactly like a deck that was never built.
    //
    // That same ordering — art before the game list — is what the fake-library dev hook does,
    // and it MASKS the shelf's entrance defect (art is already decoded on the first Ready
    // frame, so the entrance arms immediately). These scenes are evidence about the collection
    // TILE and nothing else; do not read them as saying the entrance is well.
    for (name, palette) in [
        ("07b-collections", "violet"),
        ("07b-collections-mint", "mint"),
    ] {
        let (mut s3, _c3, _l3) = collections_shell();
        s3.settings.ui_palette = palette.to_string();
        dump(&mut s3, 12, 8, &format!("_{name}-decode"), true);
        s3.handle_menu(MenuEvent::Secondary);
        dump(&mut s3, 40, 8, name, true);
    }
    // …and the same screen with NOTHING decoded: the ghost slots and the monogram badge, which
    // is the permanent look of a platform full of art-less ROM entries rather than a loading
    // state. Pale, because that is where a hardcoded face strands its own initials.
    {
        let (mut s3, _c3, _l3) = collections_shell_no_art();
        s3.settings.ui_palette = "mint".to_string();
        dump(&mut s3, 12, 8, "_noart-settle", true);
        s3.handle_menu(MenuEvent::Secondary);
        dump(&mut s3, 40, 8, "07b-collections-noart", true);
    }

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

/// A 2:3 poster, PNG-encoded, in a colour derived from `seed`.
///
/// Real encoded bytes rather than a stub, because the thing under test is the decode path:
/// `LibraryScreen` feeds these to `Image::from_encoded`, and a shape that fails to decode is
/// indistinguishable in a screenshot from a tile that chose to draw no cover.
fn poster_png(seed: usize) -> Vec<u8> {
    let mut surface = skia_safe::surfaces::raster_n32_premul((60, 90)).unwrap();
    let hue = [
        (0.85, 0.30, 0.35),
        (0.30, 0.55, 0.85),
        (0.35, 0.75, 0.45),
        (0.85, 0.65, 0.25),
    ][seed % 4];
    surface
        .canvas()
        .clear(skia_safe::Color4f::new(hue.0, hue.1, hue.2, 1.0));
    // A darker band across the lower third, so a cover is visibly ORIENTED — a flat colour
    // would hide a cover drawn upside-down or with its aspect wrong.
    surface.canvas().draw_rect(
        skia_safe::Rect::from_xywh(0.0, 62.0, 60.0, 28.0),
        &crate::theme::fill(skia_safe::Color4f::new(
            hue.0 * 0.45,
            hue.1 * 0.45,
            hue.2 * 0.45,
            1.0,
        )),
    );
    surface
        .image_snapshot()
        .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
        .unwrap()
        .as_bytes()
        .to_vec()
}

/// Games across four platforms — what the collections screen is for, and what the flat
/// `platform: None` library above cannot produce.
fn platform_games() -> Vec<crate::library::LibraryGame> {
    [
        ("Gran Turismo 6", "PlayStation 3"),
        ("The Last of Us", "PlayStation 3"),
        ("Demon's Souls", "PlayStation 3"),
        ("Halo 3", "Xbox 360"),
        ("Fable II", "Xbox 360"),
        ("Super Metroid", "SNES"),
        ("Chrono Trigger", "SNES"),
        ("Sonic 2", "Mega Drive"),
    ]
    .iter()
    .enumerate()
    .map(|(i, (title, platform))| crate::library::LibraryGame {
        id: format!("rom:{i}"),
        title: (*title).to_string(),
        store: "rom-manager".into(),
        launcher: false,
        icon: String::new(),
        platform: Some((*platform).to_string()),
        running: false,
    })
    .collect()
}

fn collections_shell_inner(
    with_art: bool,
) -> (Shell, ConsoleShared, crate::library::LibraryShared) {
    fake_home();
    let library = crate::library::LibraryShared::default();
    let games = platform_games();
    if with_art {
        for (i, g) in games.iter().enumerate() {
            library.push_art(g.id.clone(), poster_png(i));
        }
    }
    library.set_games(games);
    let console = ConsoleShared::default();
    console.set_hosts(hosts());
    let sh = Shell::new(
        console.clone(),
        library.clone(),
        ConsoleBus::default(),
        ConsoleOptions::desktop("deck".into(), false),
        vec![
            Screen::Home(HomeScreen::new()),
            Screen::Library(LibraryScreen::new(&hosts()[0], 0)),
        ],
    )
    .unwrap();
    (sh, console, library)
}

fn collections_shell() -> (Shell, ConsoleShared, crate::library::LibraryShared) {
    collections_shell_inner(true)
}

fn collections_shell_no_art() -> (Shell, ConsoleShared, crate::library::LibraryShared) {
    collections_shell_inner(false)
}

/// The bounding box of everything lit on a raster surface, in pixels: `(left, right, bottom)`.
/// White ink on a cleared black field, so any channel answers.
fn ink_bounds(surface: &mut skia_safe::Surface, w: i32, h: i32) -> (i32, i32, i32) {
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    let info = skia_safe::ImageInfo::new_n32_premul((w, h), None);
    assert!(
        surface.read_pixels(&info, &mut pixels, (w * 4) as usize, (0, 0)),
        "raster surface read-back"
    );
    let (mut left, mut right, mut bottom) = (i32::MAX, i32::MIN, i32::MIN);
    for (i, px) in pixels.chunks_exact(4).enumerate() {
        if px[0] > 60 {
            let (x, y) = (i as i32 % w, i as i32 / w);
            left = left.min(x);
            right = right.max(x);
            bottom = bottom.max(y);
        }
    }
    assert!(left <= right, "nothing was drawn");
    (left, right, bottom)
}

/// A screen heading starts on its column and stays on ONE line.
///
/// Both halves are the defect this replaced. The heading used to be centred, which read as a
/// floating label rather than as a section heading — every other punktfunk client anchors it
/// to the leading edge — and, being a wrapping paragraph, a long host name grew a SECOND line
/// downward into the screen's content. Asserted against a control render of the same string
/// with room to spare rather than against a pixel row, so the line box is Geist's to define:
/// the clamped heading must occupy the same one line the unclamped one does.
#[test]
fn a_heading_starts_on_its_column_and_never_takes_a_second_line() {
    let fonts = crate::theme::build_fonts().unwrap();
    let (w, h) = (1200, 200);
    let (x, y, size) = (crate::theme::EDGE_INSET, 18.0, 30.0);
    let title = "Living Room PC · Performance · PlayStation 3";
    let render = |max_w: f64| {
        let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
        surface
            .canvas()
            .clear(skia_safe::Color4f::new(0.0, 0.0, 0.0, 1.0));
        fonts.heading(
            surface.canvas(),
            title,
            crate::theme::W::Bold,
            size,
            skia_safe::Color4f::new(1.0, 1.0, 1.0, 1.0),
            x,
            y,
            max_w,
        );
        ink_bounds(&mut surface, w, h)
    };

    // Room to spare: one line, and the ink begins at the column (a cap's left sidebearing
    // puts it a pixel or two right of the paragraph's origin, never left of it).
    let (loose_left, loose_right, loose_bottom) = render(1100.0);
    assert!(
        (loose_left as f64) >= x - 1.0 && (loose_left as f64) < x + 0.1 * 1100.0,
        "heading ink starts at {loose_left}, which is not the {x} column"
    );
    assert!(
        loose_right < w,
        "the control render was clipped by the surface"
    );

    // Squeezed: it ellipsizes inside the budget instead of wrapping, so its ink ends where
    // the budget does and its bottom stays on the control's single line.
    let budget = 300.0;
    let (tight_left, tight_right, tight_bottom) = render(budget);
    assert_eq!(
        tight_left, loose_left,
        "clamping the width must not move the heading's left edge"
    );
    assert!(
        (tight_right as f64) <= x + budget + 1.0,
        "heading ran to {tight_right}, past its {} budget",
        x + budget
    );
    assert!(
        tight_bottom <= loose_bottom + 1,
        "heading wrapped to a second line: it reaches {tight_bottom} where one line ends at \
         {loose_bottom}"
    );
}

/// The console's geometry is ANTI-ALIASED — the defect this pins shipped in the overhaul and
/// was only caught by looking at a Deck.
///
/// Skia defaults `SkPaint::fAntiAlias` to FALSE, so `Paint::new(colour, None)` — the terse and
/// obvious way to write a draw call — produces hard-stepped edges. The console drew nearly
/// everything that way: glass panels, the badge round-rects, the online pip, the D-pad and
/// PlayStation glyph paths. Only paints that happened to be mutated for some other reason (a
/// stroke style, a width) had picked up a `set_anti_alias(true)` along the way, which is why
/// the console shipped smooth 1 px rings sitting on top of jagged fills.
///
/// Asserted on a SHAPE rather than on a screen: a full render is a poor witness here — one
/// jagged corner is a few dozen pixels in 1.02 M, and no threshold that catches it survives an
/// unrelated palette tweak. A lone circle on a blank field is unambiguous. With AA its boundary
/// is a ring of PARTIAL coverage; without it every pixel is one of exactly two values.
#[test]
fn geometry_is_anti_aliased() {
    let (w, h) = (64, 64);
    let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
    surface
        .canvas()
        .clear(skia_safe::Color4f::new(0.0, 0.0, 0.0, 1.0));
    // Deliberately off the pixel grid: a circle centred on a half-pixel has an edge that
    // cannot be represented exactly, which is when AA is the whole difference.
    surface.canvas().draw_circle(
        skia_safe::Point::new(31.5, 31.5),
        20.3,
        &crate::theme::fill(skia_safe::Color4f::new(1.0, 1.0, 1.0, 1.0)),
    );

    let mut pixels = vec![0u8; (w * h * 4) as usize];
    let info = skia_safe::ImageInfo::new_n32_premul((w, h), None);
    assert!(
        surface.read_pixels(&info, &mut pixels, (w * 4) as usize, (0, 0)),
        "raster surface read-back"
    );
    // Red channel alone — the fill is white on black, so all three agree.
    let partial = pixels
        .chunks_exact(4)
        .filter(|px| (8..248).contains(&px[0]))
        .count();
    assert!(
        partial > 40,
        "an anti-aliased circle of r≈20 has a boundary ring of partially covered pixels; found \
         {partial}, which is what `Paint::new`'s aliased default looks like"
    );
}

/// A shader-painted element actually PAINTS — the second trap in the same corner, and the one
/// that cost a whole screenshot round.
///
/// Skia modulates a shader's output by the paint's ALPHA. `Paint::default` is opaque black, so
/// the console's gradients and the aurora's runtime effect never noticed the rule existed; the
/// moment those paints were rebuilt from a "the shader supplies the colour anyway" transparent
/// placeholder, every one of them drew NOTHING. Not dimmer, not wrong-coloured — absent: the
/// backdrop, the badge, the vignette and the panel's gradient stroke all vanished at once, and
/// every test still passed, because a test that only renders a frame cannot tell a missing layer
/// from a dark one. `theme::shaded` is opaque by construction; this holds it to that.
#[test]
fn a_shaded_paint_is_opaque_enough_to_draw() {
    let (w, h) = (32, 32);
    let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
    surface
        .canvas()
        .clear(skia_safe::Color4f::new(0.0, 0.0, 0.0, 1.0));
    let mut p = crate::theme::shaded();
    let stops = [
        skia_safe::Color4f::new(1.0, 1.0, 1.0, 1.0),
        skia_safe::Color4f::new(1.0, 1.0, 1.0, 1.0),
    ];
    p.set_shader(skia_safe::gradient::shaders::linear_gradient(
        (
            skia_safe::Point::new(0.0, 0.0),
            skia_safe::Point::new(0.0, h as f32),
        ),
        &skia_safe::gradient::Gradient::new(
            skia_safe::gradient::Colors::new_evenly_spaced(
                &stops,
                skia_safe::TileMode::Clamp,
                None,
            ),
            skia_safe::gradient::Interpolation::default(),
        ),
        None,
    ));
    surface
        .canvas()
        .draw_rect(skia_safe::Rect::from_wh(w as f32, h as f32), &p);

    let mut pixels = vec![0u8; (w * h * 4) as usize];
    let info = skia_safe::ImageInfo::new_n32_premul((w, h), None);
    assert!(
        surface.read_pixels(&info, &mut pixels, (w * 4) as usize, (0, 0)),
        "raster surface read-back"
    );
    let lit = pixels.chunks_exact(4).filter(|px| px[0] > 200).count();
    assert_eq!(
        lit,
        (w * h) as usize,
        "an opaque white gradient over the whole surface should light every pixel; a paint \
         whose own alpha is 0 scales the shader away and leaves the field black"
    );
}

/// …and every paint in the crate is built by `theme::fill`/`stroke`/`layer`, so the assertion
/// above keeps holding for code written after it.
///
/// A pixel test can only witness the shapes it happens to draw; this witnesses the CLASS. The
/// trap is that the aliased spelling is the NATURAL one — `&Paint::new(c, None)` passed inline
/// as an argument, no binding, no obvious place to hang a flag — so it reappears whenever a new
/// draw call is written, in whichever file is being worked on that day. Reading the crate's own
/// source is the only check that scales to that.
#[test]
fn paints_are_built_by_the_theme_constructors() {
    // Split so the needles do not appear literally in this file — the scan reads its own
    // source too, and a self-match is the first thing this test did.
    let needles = [concat!("Paint", "::new("), concat!("Paint", "::default()")];
    let mut offenders = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src"
    ))];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("the crate's own src is readable") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            // theme.rs holds the sanctioned constructors, and is the one place the raw ones
            // are allowed.
            if path.file_name().is_some_and(|f| f == "theme.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("source is UTF-8");
            for (n, line) in text.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") || code.starts_with('*') {
                    continue;
                }
                if needles.iter().any(|needle| code.contains(needle)) {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    offenders.push(format!("{name}:{}: {code}", n + 1));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these build a Skia paint directly, which means anti-aliasing is OFF on whatever they \
         draw — use `theme::fill`, `theme::stroke`, or `theme::layer` for a `save_layer` \
         paint:\n  {}",
        offenders.join("\n  ")
    );
}
