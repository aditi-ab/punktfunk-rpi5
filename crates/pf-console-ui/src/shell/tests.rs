use super::*;

#[test]
fn the_os_field_compiles_for_both_modes() {
    for light in [false, true] {
        let t = crate::os_theme::OsTheme {
            light,
            background: if light {
                (0.99, 0.96, 0.89)
            } else {
                (0.02, 0.04, 0.12)
            },
            foreground: if light {
                (0.36, 0.42, 0.45)
            } else {
                (1.0, 0.81, 0.68)
            },
            accent: (0.49, 0.51, 0.85),
        };
        build_mesh_os(&t).unwrap();
    }
}
use crate::model::WakeStatus;
use crate::screens::home::HomeScreen;
use crate::screens::library::LibraryScreen;
use punktfunk_core::config::GamepadPref;

/// Pins `motion_spring` (vectors v2). v1 `motion` still exists for other clients; this
/// transition is a spring, not that ease-out, so sampling v1 would pass a curve we do not run.
///
/// Springs are integrator-dependent: two runtimes that honour `response`/`damping` agree
/// to the eye and disagree in the third decimal. Pin the parameters, not sampled positions.
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

    // v1 stays until the last client migrates. Deleting it here reds Android's test.
    assert!(
        file["motion"]["$deprecated"].is_string(),
        "the v1 motion block must carry its deprecation note while other clients read it"
    );
}

/// Shared throwaway config dir. Settings SAVE on adjust; a second `OnceLock` here
/// would pick a second directory and the other test's loads would miss its writes.
use crate::screens::settings::tests::fake_home;

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
        actions: Vec::new(),
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

#[test]
fn navigation_lap() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.handle_menu(MenuEvent::Tertiary);
    assert_eq!(s.stack.len(), 2);
    finish_motion(&mut s);
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    s.handle_menu(MenuEvent::Back);
    finish_motion(&mut s);
    assert_eq!(s.stack.len(), 1);
    s.handle_menu(MenuEvent::Secondary);
    assert_eq!(s.stack.len(), 2);
    finish_motion(&mut s);
    s.handle_menu(MenuEvent::Back);
    finish_motion(&mut s);
    assert_eq!(s.stack.len(), 1);
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
    // Cancel drops the takeover immediately. Waiting for a session phase burns the
    // connect budget; an embedder that drops a canceled dial sends no phase at all.
    s.handle_menu(MenuEvent::Back);
    assert!(matches!(
        s.take_action(),
        Some(OverlayAction::CancelConnect)
    ));
    assert!(s.connecting.is_none(), "cancel drops the takeover itself");
    s.session_ended(None);
    assert!(s.connecting.is_none());
}

fn finish_motion(s: &mut Shell) {
    // Seat the spring and run the real settle. Skipping it drops the bookkeeping
    // that pops a reversed push off the stack.
    if let Motion::Nav { spring, target, .. } = &mut s.motion {
        spring.pos = *target;
        spring.vel = 0.0;
    }
    s.finish_nav();
}

/// Step at a fixed `dt` until settle. Bound so a spring that never settles fails
/// instead of hanging.
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

/// Y on a pinned card must carry that profile into the library. Falling back to the
/// host default would ignore the pin, which is why the card exists.
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

    // Pinned card sits immediately after its host's primary tile.
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

/// Primary tile: no one-off profile. The resolver sees `None` and uses the host binding.
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
    // Office Tower is the second tile: offline and wakeable.
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    s.handle_menu(MenuEvent::Confirm);
    let w = s
        .wake
        .as_ref()
        .expect("Waking card raised in the SAME call as the A press");
    assert_eq!(w.name, "Office Tower");
    assert!(!w.online);
    // Gate the next input. `sync` (first in handle_menu) must not clear the
    // placeholder before the service thread reports a real status.
    assert!(s.handle_menu(MenuEvent::Move(MenuDir::Right)).is_none());
    assert!(
        s.wake.is_some(),
        "optimistic card survived a sync with no service status"
    );
    s.handle_menu(MenuEvent::Back);
    assert!(s.wake.is_none());
    assert!(s.handle_menu(MenuEvent::Move(MenuDir::Left)).is_some());
}

/// Tab / Shift+Tab change section even with a pad attached. The legend names
/// PgUp/PgDn only when no pad is present, so keyboard users otherwise have no way in.
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
    s.key(Scancode::Tab, true, false);
    assert_eq!(tab(&s), crate::screens::settings::TAB_COUNT - 1);
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
    // Same transition a B press uses.
    assert!(matches!(
        s.motion,
        Motion::Nav {
            kind: NavKind::Pop,
            ..
        }
    ));
}

/// Replace recedes the swapped-out screen, not its parent. A push paints the
/// screen beneath as the leaving layer; replace already popped, so without carrying
/// the predecessor the renderer recedes the parent. Asserted on the carried screen:
/// a frame diff would pin the look of a transition, not which screen it holds.
#[test]
fn a_replace_carries_the_screen_it_replaced() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.handle_menu(MenuEvent::Move(MenuDir::Up));
    assert!(matches!(s.stack.last(), Some(Screen::HostOptions(_))));
    finish_motion(&mut s);

    // First host's menu is [Send logs, Library, Copy link, Edit…, …] — three Downs.
    // Pressed exactly so a menu reorder fails here, not on something destructive.
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
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

    s.handle_menu(MenuEvent::Back);
    finish_motion(&mut s);
    assert!(
        matches!(s.stack.last(), Some(Screen::HostOptions(_))),
        "a reversed replace lands where the user actually was"
    );
}

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
    // One lap of the strip. Walk to the end first so both row paths run, then one
    // frame per tab — every tab's rows fit on 800-tall. CPU SkSL is ~1s/frame in
    // debug; this is a panic catch, not an eyeball pass.
    for _ in 0..crate::screens::settings::TAB_COUNT {
        for _ in 0..12 {
            s.handle_menu(MenuEvent::Move(MenuDir::Down));
        }
        frame(&mut s);
        s.handle_menu(MenuEvent::JumpForward);
    }
    // 640×400: pills are measured text, so a too-small width must clamp, not overflow.
    s.render(surface.canvas(), 640, 400, &fonts, None, None, &pads);
}

/// One rendered frame so the rows have real rects to press.
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

/// A finger swipe across the list is a scroll and must not flip the landed-on value.
/// The same contact lifted in place is the tap, delivered on lift at the anchor.
#[test]
fn a_touch_swipe_scrolls_settings_without_changing_a_value() {
    use pf_client_core::console::{PointerButton, PointerInput};
    let (mut s, row) = rendered_settings();
    let (cx, cy) = (row.center_x(), row.center_y());
    // Resolution's observable: Native → Match window flips the flag; size stays (0, 0).
    let state = |s: &Shell| (s.settings.match_window, s.settings.width, s.settings.height);
    let before = state(&s);

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

/// A mouse press still acts on contact. Only touch defers to the lift.
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

/// One tick per `DRAG_TICK_DP` past slop; the lift after a drag presses nothing.
/// Ticks act on the cursor, not drawn rects, so no render.
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
    // First move leaves slop (locks X); the second is one tick left — next tile.
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
    // Second host is offline with a stored MAC: Confirm raises wake. That proves
    // the drag moved the cursor and the lift itself pressed nothing.
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

/// A canceled touch is dropped whole: a stray lift after Cancel must not act.
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

/// Back mid-push retargets the same spring rather than queuing a pop. Cancel-and-play
/// snaps because the two recipes disagree on position; carrying `pos` cannot.
/// Assert the first sample after retarget is within a frame of travel.
#[test]
fn back_mid_push_turns_the_screen_around() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.handle_menu(MenuEvent::Tertiary); // X → Settings
    assert_eq!(s.stack.len(), 2);

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
    assert!(
        (path[0] - before).abs() < 0.05,
        "jumped from {before} to {}",
        path[0]
    );
    assert!(*path.last().expect("non-empty") < before);
    assert_eq!(
        s.stack.len(),
        1,
        "the reversed push took its screen back off"
    );
    assert!(matches!(s.motion, Motion::None));
}

/// Back at the root is not a reversal: there is no parent, and B there means quit.
/// Decline it so the normal path can answer.
#[test]
fn back_mid_push_at_the_root_is_left_to_the_normal_path() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    // Replace at the root pushes without deepening the stack.
    s.apply_nav(crate::screens::Nav::Replace(Box::new(Screen::Home(
        HomeScreen::new(),
    ))));
    assert_eq!(s.stack.len(), 1);
    s.advance_nav(1.0 / 120.0);
    assert!(!s.nav_back(), "nothing to reverse into");
    finish_motion(&mut s);
    assert_eq!(s.stack.len(), 1, "and the root survived");
}

/// Mid-pop Confirm is a mis-tap and is refused. Mid-pop Back is a held B: start
/// the next pop at once rather than queuing it behind the current one.
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

    s.handle_menu(MenuEvent::Back);
    finish_motion(&mut s);
    assert!(matches!(s.take_action(), Some(OverlayAction::Quit)));
}

/// A completed pop frees the carried screen. Hint rects publish only at
/// `Motion::None` — mid-transition they are slid and scaled.
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

/// One small frame. A freshly pushed screen adopts the shared model on its first
/// sync; asserting on content before that is asking before the answer exists.
fn frame(s: &mut Shell) {
    let fonts = crate::theme::build_fonts().unwrap();
    let pads: Vec<PadInfo> = Vec::new();
    let mut surface = skia_safe::surfaces::raster_n32_premul((480, 300)).unwrap();
    s.render(surface.canvas(), 480, 300, &fonts, None, None, &pads);
}

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

/// Asserted on the shell stack after `sync`. A screen cannot replace itself;
/// only the shell owns the stack, so that is where the handover has to be witnessed.
#[test]
fn the_setting_hands_a_multi_platform_library_over_to_collections() {
    let games: Vec<crate::library::LibraryGame> = platform_games();
    for (want_collections, enabled) in [(true, true), (false, false)] {
        let (mut s, _console, library) = shell(vec![
            Screen::Home(HomeScreen::new()),
            Screen::Library(LibraryScreen::new(&hosts()[0], 0)),
        ]);
        s.settings.library_collections = enabled;

        // Ready before `begin_fetch` is the previous host's library. The epoch
        // `begin_fetch` raises is what the shelf compares against its push epoch.
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

/// A collections screen listing a single tile is a press that buys nothing.
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

#[test]
fn collections_drill_in_reaches_one_platform_and_backs_out() {
    let (mut s, _console, library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    mixed_library(&library);
    s.handle_menu(MenuEvent::Secondary); // Y at home → this host's library
    finish_motion(&mut s);
    assert!(matches!(s.stack.last(), Some(Screen::Library(_))));

    s.handle_menu(MenuEvent::Secondary);
    finish_motion(&mut s);
    assert!(
        matches!(s.stack.last(), Some(Screen::Collections(_))),
        "Y on a multi-group library opens the collections"
    );

    // Groups sort A–Z with launchers first: Launchers, PS3, SNES, Steam.
    s.handle_menu(MenuEvent::Move(MenuDir::Right));

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

/// A library with nothing to collect must not offer the button, and must not answer it.
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

    mixed_library(&library);
    s.handle_menu(MenuEvent::Secondary);
    finish_motion(&mut s);
    assert!(matches!(s.stack.last(), Some(Screen::Collections(_))));
}

/// Rescan sits past Add Host and must never start a session: accidental A on the
/// end of the strip raises a scan, not a Launch.
#[test]
fn the_rescan_tile_probes_and_never_connects() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    // Hosts, then Add Host, then Rescan — walk to the end.
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
    // One step back is Add Host, which does push — proof the walk reached the end.
    s.handle_menu(MenuEvent::Move(MenuDir::Left));
    s.handle_menu(MenuEvent::Confirm);
    assert!(
        matches!(s.stack.last(), Some(Screen::AddHost(_))),
        "the tile before Rescan is Add Host"
    );
}

/// Error tint is fixed, not palette-derived. `moss` accent is green; reporting
/// a failure in the colour the rest of the UI uses for "this is fine" is the bug.
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

    // Green-accented palette: Success follows it, Error must not.
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

/// Reduced motion freezes `field_clock` and shortens the spring. Asserted on the
/// clock, not pixels: `draw_aurora` has one time read, and both callers go through it.
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
    // Shader still draws at t = 0.
    s.render(surface.canvas(), w, h, &fonts, None, None, &pads);

    // Persist through the settings file so a restart keeps it.
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

/// Ignored eyeball dump. `PF_CONSOLE_DUMP=<dir> cargo test -p pf-console-ui --release -- --ignored dump`.
/// CPU raster: SkSL aurora, layers, and text run without a GPU.
#[test]
#[ignore]
fn dump_console_screens() {
    let dir = std::env::var("PF_CONSOLE_DUMP").expect("set PF_CONSOLE_DUMP to an output dir");
    let fonts = crate::theme::build_fonts().unwrap();
    let (w, h) = (1280, 800);
    let pads: Vec<PadInfo> = Vec::new();
    let dump = |shell: &mut Shell, frames: usize, sleep_ms: u64, name: &str, pad: bool| {
        // Fixed step = sleep + ~4 ms raster, so two dumps compare independent of load.
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

    let (mut s, console, library) = shell(vec![Screen::Home(HomeScreen::new())]);
    dump(&mut s, 40, 8, "01-home", true);

    // Up on the focused saved tile. Eyeball with 01-home: that frame carries the Options hint.
    s.handle_menu(MenuEvent::Move(MenuDir::Up));
    dump(&mut s, 40, 8, "01b-host-options", true);
    s.handle_menu(MenuEvent::Back);
    dump(&mut s, 20, 8, "_settle0", true);

    // A few fast frames land around p ≈ 0.4 — both layers visible.
    s.handle_menu(MenuEvent::Tertiary);
    dump(&mut s, 3, 25, "02-transition", true);
    dump(&mut s, 40, 8, "03-settings", true);

    // Interface tab leads with Background. Palettes are set by id, not Confirm counts,
    // so reordering the table cannot shoot the wrong one. Accent, ink and scrim move
    // together: pale palettes need dark text on them.
    for _ in 0..5 {
        s.handle_menu(MenuEvent::JumpForward);
    }
    for id in ["violet", "oled", "ember", "abyss", "holo", "sunset", "mint"] {
        s.settings.ui_palette = id.to_string();
        dump(&mut s, 40, 8, &format!("03-settings-{id}"), true);
    }
    for _ in 0..5 {
        s.handle_menu(MenuEvent::JumpBack);
    }
    // Home at full contrast under a few palettes: the backdrop's loudest form.
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

    // Add Host with the keyboard tray; no pad so the glyphs are keyboard-style.
    s.handle_menu(MenuEvent::Back);
    dump(&mut s, 40, 8, "_back", true);
    for _ in 0..3 {
        s.handle_menu(MenuEvent::Move(MenuDir::Right));
    }
    s.handle_menu(MenuEvent::Confirm);
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

    s.handle_menu(MenuEvent::Back); // close keyboard
    s.handle_menu(MenuEvent::Back); // leave add-host
    dump(&mut s, 40, 8, "_back2", true);
    s.handle_menu(MenuEvent::Move(MenuDir::Left)); // onto "steambox"
    s.handle_menu(MenuEvent::Confirm);
    dump(&mut s, 40, 8, "06-pair", true);

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
    // Fresh shell per scene: entrance and bar focus are per-shell and cannot be rewound.
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
    // 80 frames, not 40: no art means the 400 ms art-wait deadline, and 40×8 ms can
    // finish inside it and dump the spinner as the coverflow.
    dump(&mut s2, 80, 8, "07-library", true);

    // Sort/view bar focused: the only state that draws the accent wash. Both palette
    // poles — `accent(0.14)` reads differently over dark than pale.
    for (name, palette) in [
        ("07c-library-bar", "violet"),
        ("07c-library-bar-mint", "mint"),
    ] {
        let mut s4 = shelf_shell();
        s4.settings.ui_palette = palette.to_string();
        // Settle the shelf first (same 400 ms art-wait). Up before the field exists is swallowed.
        dump(&mut s4, 80, 8, "_07c-settle", true);
        s4.handle_menu(MenuEvent::Move(MenuDir::Up));
        dump(&mut s4, 20, 8, name, true);
    }

    // `adopt_art` is a one-shot at the Y press: push art and give the shelf frames to
    // decode it first, or every tile is a monogram. Art-before-list is also what the
    // fake-library hook does, which masks the entrance defect — these scenes are about
    // the collection tile, not the entrance.
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
    // Nothing decoded: ghost slots and the monogram badge — the permanent look of
    // art-less ROM entries. Pale, where a hardcoded face strands its initials.
    {
        let (mut s3, _c3, _l3) = collections_shell_no_art();
        s3.settings.ui_palette = "mint".to_string();
        dump(&mut s3, 12, 8, "_noart-settle", true);
        s3.handle_menu(MenuEvent::Secondary);
        dump(&mut s3, 40, 8, "07b-collections-noart", true);
    }

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

    // Android + keys: OK/↩ badges, section pointer, hidden Y/X, remote chip. Platform
    // flip is legends-only; the stack was built desktop.
    dump(&mut s, 30, 8, "_remote-settle", true);
    s.platform = crate::platform::Platform::Android;
    s.note_input_source(crate::console::InputSource::Keys);
    dump(&mut s, 40, 8, "11-home-remote", false);
    s.handle_menu(MenuEvent::Tertiary);
    dump(&mut s, 40, 8, "11b-settings-remote", false);
}

/// A 2:3 poster, PNG-encoded, colour from `seed`. Real bytes: `LibraryScreen` feeds
/// these to `Image::from_encoded`, and a decode miss looks like a tile with no cover.
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
    // Darker band on the lower third so a flipped or wrong-aspect cover is visible.
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

/// Bounding box of lit pixels: `(left, right, bottom)`. White ink on black, any channel.
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

/// Compared to an unclamped control of the same string so Geist defines the line
/// box: the clamped heading must occupy the same one line the unclamped one does.
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

    // One line; a cap's left sidebearing sits a pixel or two right of origin, never left.
    let (loose_left, loose_right, loose_bottom) = render(1100.0);
    assert!(
        (loose_left as f64) >= x - 1.0 && (loose_left as f64) < x + 0.1 * 1100.0,
        "heading ink starts at {loose_left}, which is not the {x} column"
    );
    assert!(
        loose_right < w,
        "the control render was clipped by the surface"
    );

    // Ellipsize inside the budget instead of wrapping: right edge at the budget, same bottom.
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

/// Skia defaults `SkPaint::fAntiAlias` to false, so `Paint::new(colour, None)` hard-steps.
/// Asserted on a lone circle: a full render hides a few dozen jagged pixels in 1.02 M,
/// and no threshold that catches them survives a palette tweak. With AA the boundary
/// is a ring of partial coverage; without it every pixel is one of two values.
#[test]
fn geometry_is_anti_aliased() {
    let (w, h) = (64, 64);
    let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
    surface
        .canvas()
        .clear(skia_safe::Color4f::new(0.0, 0.0, 0.0, 1.0));
    // Off the pixel grid: a half-pixel centre has an edge that cannot be exact, so AA matters.
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
    // White on black: all three channels agree, so red alone is enough.
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

/// Skia modulates a shader by the paint's alpha. `Paint::default` is opaque black so
/// gradients never noticed; a transparent "shader supplies the colour" placeholder
/// draws nothing. `theme::shaded` is opaque by construction; this holds it to that.
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

/// Every paint in the crate is built by `theme::fill`/`stroke`/`layer`. A pixel test
/// only witnesses the shapes it draws; this witnesses the class. `&Paint::new(c, None)`
/// is the natural inline spelling, so it reappears in whichever file is being written.
#[test]
fn paints_are_built_by_the_theme_constructors() {
    // Concat so the needles do not appear in this file — the scan reads its own source.
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
            // theme.rs holds the sanctioned constructors; raw paints are allowed there only.
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
