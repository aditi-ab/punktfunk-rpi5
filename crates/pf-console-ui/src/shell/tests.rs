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
    assert!(matches!(
        s.motion,
        Motion::Nav {
            kind: NavKind::Pop,
            ..
        }
    ));
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
