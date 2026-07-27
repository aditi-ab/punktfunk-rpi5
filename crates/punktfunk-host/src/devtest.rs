//! Standalone dev/test subcommands that validate a subsystem without a streaming client: the
//! input-injection smoke test and the virtual-gamepad exercisers (Linux UHID DualSense /
//! Switch Pro; Windows UMDF DualSense-family + the Steam Deck devnode spike). Split out of the
//! `main` CLI dispatch (plan §W5 "devtest.rs, land first") so `main.rs`'s match keeps only thin
//! arms that forward here. Each fn owns the full behaviour (and doc) of its former inline arm.

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;

/// Draw a scripted stylus stroke through the REAL pen chain — wire-shaped samples → the core
/// [`PenTracker`](punktfunk_core::quic::PenTracker) → the "Punktfunk Pen" uinput tablet — so
/// full-fidelity pen injection is validated without any client (design/pen-tablet-input.md P1):
/// hover in from the left, tip down, a sine stroke across the mapped output with a pressure
/// ramp + tilt sweep, tip up, hover out. Observe in Krita/GIMP with a pressure brush, or
/// `sudo libinput debug-events` (expect `TABLET_TOOL_PROXIMITY/TIP/AXIS`).
#[cfg(target_os = "linux")]
pub fn pen_test() -> Result<()> {
    use punktfunk_core::quic::{
        PenBatch, PenSample, PenTracker, PenTransition, PEN_IN_RANGE, PEN_TOUCHING,
    };
    use std::time::Duration;

    let mut dev = crate::inject::pen::VirtualPen::create()?;
    let mut tracker = PenTracker::default();
    let mut out: Vec<PenTransition> = Vec::new();
    // Compositors need a beat to enumerate the new evdev node before events count.
    std::thread::sleep(Duration::from_secs(2));

    let mut seq = 0u16;
    let mut send = |tracker: &mut PenTracker, out: &mut Vec<PenTransition>, s: PenSample| {
        out.clear();
        tracker.apply(&PenBatch::new(seq, &[s]), out);
        seq = seq.wrapping_add(1);
        dev.apply_batch(out);
    };

    tracing::info!("pen-test: hover in, then a 3 s pressure-ramped sine stroke");
    let hover = |x: f32| PenSample {
        state: PEN_IN_RANGE,
        x,
        y: 0.5,
        distance: 300,
        ..Default::default()
    };
    for i in 0..20 {
        send(&mut tracker, &mut out, hover(0.05 + i as f32 * 0.005));
        std::thread::sleep(Duration::from_millis(10));
    }
    const STEPS: u32 = 360;
    for i in 0..=STEPS {
        let t = i as f32 / STEPS as f32;
        send(
            &mut tracker,
            &mut out,
            PenSample {
                state: PEN_IN_RANGE | PEN_TOUCHING,
                x: 0.15 + 0.7 * t,
                y: 0.5 + 0.2 * (t * std::f32::consts::TAU * 2.0).sin(),
                // Ramp 10 % → 100 % so a pressure brush visibly widens along the stroke.
                pressure: (6553.0 + 58982.0 * t) as u16,
                distance: 0,
                tilt_deg: 25 + (20.0 * t) as u8,
                azimuth_deg: ((90.0 + 180.0 * t) as u16) % 360,
                roll_deg: ((360.0 * t) as u16) % 360,
                ..Default::default()
            },
        );
        std::thread::sleep(Duration::from_millis(8));
    }
    for i in 0..10 {
        send(&mut tracker, &mut out, hover(0.85 + i as f32 * 0.005));
        std::thread::sleep(Duration::from_millis(10));
    }
    send(&mut tracker, &mut out, PenSample::default()); // state 0 = out of range
    tracing::info!("pen-test: done (stroke drawn, pen out of range) — device destroyed on exit");
    Ok(())
}

/// Inject a scripted mouse + keyboard pattern through the session's input backend (libei on
/// KWin/GNOME, wlr on Sway). Lets us validate input injection without a Moonlight client.
#[cfg(target_os = "linux")]
pub fn input_test() -> Result<()> {
    use punktfunk_core::input::{InputEvent, InputKind};
    use std::time::Duration;

    let backend = crate::inject::default_backend();
    tracing::info!(?backend, "input-test: opening injector");
    let mut inj = crate::inject::open(backend)?;
    // An async backend (libei) needs a moment to establish its portal/EIS session + device
    // resume; events injected before then are dropped.
    std::thread::sleep(Duration::from_secs(4));

    let ev = |kind, code, x, y| InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y,
        flags: 0,
    };
    // `PUNKTFUNK_INPUT_TEST_ABS=WxH` (e.g. 1280x800): exercise ABSOLUTE pointer moves instead —
    // steps through the corners + center of the given surface, 1s apart, so an observer
    // (`DISPLAY=:0 xdotool getmouselocation`) can verify each jump. This is the degraded-touch
    // path (touch → MouseMoveAbs), so it validates game-mode touch without a client.
    if let Ok(dims) = std::env::var("PUNKTFUNK_INPUT_TEST_ABS") {
        let (w, h) = dims
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
            .unwrap_or((1280, 800));
        let flags = (w << 16) | (h & 0xffff);
        let pts = [
            (100, 100),
            (w as i32 - 100, 100),
            (w as i32 - 100, h as i32 - 100),
            (100, h as i32 - 100),
            (w as i32 / 2, h as i32 / 2),
        ];
        tracing::info!(w, h, "input-test: ABS mode — corners + center, 1s apart");
        for (x, y) in pts {
            let mut e = ev(InputKind::MouseMoveAbs, 0, x, y);
            e.flags = flags;
            if let Err(err) = inj.inject(&e) {
                tracing::warn!(error = %format!("{err:#}"), "input-test: abs inject failed");
            }
            tracing::info!(x, y, "input-test: abs move emitted");
            std::thread::sleep(Duration::from_secs(1));
        }
        tracing::info!("input-test: done (abs)");
        return Ok(());
    }
    tracing::info!(
        "input-test: injecting a mouse square + 'A'/click taps for ~8s (watch wev / focused app)"
    );
    for i in 0..160u32 {
        let (dx, dy) = match (i / 10) % 4 {
            0 => (12, 0),
            1 => (0, 12),
            2 => (-12, 0),
            _ => (0, -12),
        };
        if let Err(e) = inj.inject(&ev(InputKind::MouseMove, 0, dx, dy)) {
            tracing::warn!(error = %format!("{e:#}"), "input-test: inject failed");
        }
        if i % 20 == 0 {
            let _ = inj.inject(&ev(InputKind::KeyDown, 0x41, 0, 0)); // 'A'
            let _ = inj.inject(&ev(InputKind::KeyUp, 0x41, 0, 0));
            let _ = inj.inject(&ev(InputKind::MouseButtonDown, 1, 0, 0)); // left click
            let _ = inj.inject(&ev(InputKind::MouseButtonUp, 1, 0, 0));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    tracing::info!("input-test: done");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn input_test() -> Result<()> {
    anyhow::bail!("input-test requires Linux")
}

/// Create a virtual DualSense via UHID and exercise it (validation, no streaming session):
/// toggles the Cross button, sweeps the left stick, and prints any HID output the kernel
/// sends back. Verify with `evtest` / `ls /dev/input/by-id/*Punktfunk*` / `wpctl status`.
/// `--edge` creates a DualSense **Edge** (054C:0DF2) instead and additionally cycles the
/// four back/Fn buttons (kernel ≥ 7.2 exposes them as BTN_TRIGGER_HAPPY1..4; on older
/// kernels verify the bind + `hidraw` byte 10 instead).
#[cfg(target_os = "linux")]
pub fn dualsense_test(args: &[String]) -> Result<()> {
    use crate::inject::dualsense::{DsUhidIdentity, DualSensePad};
    use crate::inject::dualsense_proto::{edge_paddle_bits, DsState};
    let secs: u64 = args
        .iter()
        .skip_while(|a| *a != "--seconds")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let edge = args.iter().any(|a| a == "--edge");
    let (identity, label) = if edge {
        (DsUhidIdentity::dualsense_edge(), "DualSense Edge")
    } else {
        (DsUhidIdentity::dualsense(), "DualSense")
    };
    use std::time::{Duration, Instant};
    let mut pad = DualSensePad::open(0, &identity)
        .with_context(|| format!("create virtual {label} via /dev/uhid"))?;
    // Answer the kernel's init GET_REPORTs promptly so hid-playstation creates the input
    // devices before we start streaming state.
    let init = Instant::now() + Duration::from_millis(800);
    while Instant::now() < init {
        pad.service(0);
        std::thread::sleep(Duration::from_millis(10));
    }
    println!(
        "virtual {label} created — check `evtest`, `ls /dev/input/by-id/*Punktfunk*`, \
         `ls /sys/class/leds/`. Cycling Cross + sweeping LS for {secs}s."
    );
    let deadline = Instant::now() + Duration::from_secs(secs);
    let (mut i, mut last_write) = (0i32, Instant::now());
    while Instant::now() < deadline {
        let fb = pad.service(0);
        if let Some((low, high)) = fb.rumble {
            println!("  rumble from kernel/game: low={low} high={high}");
        }
        for o in fb.hidout {
            println!("  hid output from kernel/game: {o:?}");
        }
        if last_write.elapsed() >= Duration::from_millis(300) {
            last_write = Instant::now();
            i += 1;
            let mut buttons = if i % 2 == 0 {
                punktfunk_core::input::gamepad::BTN_A
            } else {
                0
            };
            if edge {
                // Cycle one paddle per beat (R4 → L4 → R5 → L5) so all four Edge slots
                // are visible in evtest / hidraw.
                buttons |= punktfunk_core::input::gamepad::BTN_PADDLE1 << (i % 4);
            }
            let lx = (((i % 64) - 32) * 1024) as i16; // sweep left stick X
            let mut st = DsState::from_gamepad(buttons, lx, 0, 0, 0, 0, 0);
            if edge {
                st.buttons[2] |= edge_paddle_bits(buttons);
            }
            pad.write_state(&st).context("write report")?;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    println!("dualsense-test: done");
    Ok(())
}

/// Create a virtual Switch Pro Controller via UHID and exercise it (validation, no
/// streaming session): answers the full hid-nintendo probe conversation, then cycles the
/// A/B buttons (positionally swapped) + sweeps the left stick, printing rumble / player-
/// light feedback. Verify with `evtest` (hid-nintendo input devices), `dmesg | grep
/// nintendo`, SDL identifying a "Nintendo Switch Pro Controller".
#[cfg(target_os = "linux")]
pub fn switchpro_test(args: &[String]) -> Result<()> {
    use crate::inject::switch_pro::SwitchProPad;
    use crate::inject::switch_proto::SwitchState;
    let secs: u64 = args
        .iter()
        .skip_while(|a| *a != "--seconds")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    use std::time::{Duration, Instant};
    let mut pad =
        SwitchProPad::open(0).context("create virtual Switch Pro Controller via /dev/uhid")?;
    // Answer the driver's probe conversation promptly — every step blocks hid-nintendo
    // init until its reply lands; also stream neutral 0x30 reports like real hardware.
    println!("virtual Switch Pro created — servicing the hid-nintendo probe…");
    let init = Instant::now() + Duration::from_millis(2500);
    let mut hb = Instant::now();
    while Instant::now() < init {
        let fb = pad.service(0);
        for o in fb.hidout {
            println!("  probe feedback: {o:?}");
        }
        if hb.elapsed() >= Duration::from_millis(15) {
            hb = Instant::now();
            let _ = pad.write_state(&SwitchState::neutral());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    println!("probe window over — cycling buttons + stick for {secs}s (check evtest)");
    let deadline = Instant::now() + Duration::from_secs(secs);
    let (mut i, mut last_write) = (0i32, Instant::now());
    while Instant::now() < deadline {
        let fb = pad.service(0);
        if let Some((low, high)) = fb.rumble {
            println!("  rumble from kernel/game: low={low} high={high}");
        }
        for o in fb.hidout {
            println!("  hid output from kernel/game: {o:?}");
        }
        // ~15 ms cadence = the real controller's report rate (also keeps the driver's
        // post-probe subcommand rate limiter fed).
        if last_write.elapsed() >= Duration::from_millis(15) {
            last_write = Instant::now();
            i += 1;
            let step = i / 20; // change the pressed button every ~300 ms
            let buttons = if step % 2 == 0 {
                punktfunk_core::input::gamepad::BTN_A
            } else {
                punktfunk_core::input::gamepad::BTN_B
            };
            let lx = (((i % 64) - 32) * 1024) as i16; // sweep left stick X
            let st = SwitchState::from_gamepad(buttons, lx, 0, 0, 0, 0, 0);
            pad.write_state(&st).context("write Switch Pro report")?;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    println!("switchpro-test: done");
    Ok(())
}

/// Windows N4 SPIKE (gamepad-new-types §6): hold a software-devnode HID Steam Deck
/// (28DE:1205 via device_type 3) and watch whether Steam Input promotes it. Needs the
/// updated signed driver installed + Steam running. `--seconds N` (default 120).
#[cfg(target_os = "windows")]
pub fn deck_windows_spike(args: &[String]) -> Result<()> {
    let secs: u64 = args
        .iter()
        .skip_while(|a| *a != "--seconds")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    crate::inject::dualsense_windows::deck_spike_hold(0, secs)
}

/// Windows vmouse SPIKE: hold the pf-mouse virtual HID pointer and sweep the REAL cursor via HID
/// reports — proves devnode → INF bind → mshidumdf → mouhid → win32k on-glass, and that a resident
/// virtual pointer makes `SM_MOUSEPRESENT` true (DWM then composites the cursor) with no dongle
/// attached. Run with the host service STOPPED (the resident mouse owns the mailbox otherwise).
/// `--seconds N` (default 30).
#[cfg(target_os = "windows")]
pub fn vmouse_spike(args: &[String]) -> Result<()> {
    let secs: u64 = args
        .iter()
        .skip_while(|a| *a != "--seconds")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    crate::inject::mouse_windows::spike_hold(secs)
}

/// Windows: create a virtual DualSense via the UMDF driver (a SwDeviceCreate per-session
/// devnode plus the shared-memory channel) and hold it, pushing one fixed frame (Cross +
/// LS-right). Drives the real DualSenseWindowsManager, so it validates the device lifecycle
/// end to end. Verify while it holds: `Get-PnpDevice` shows a VID_054C device, and a HID read
/// returns the pushed report (byte1=0xC0, byte8=0x28). On exit the pad drops → SwDeviceClose
/// removes the devnode.
#[cfg(target_os = "windows")]
pub fn dualsense_windows_test(args: &[String]) -> Result<()> {
    use punktfunk_core::input::{GamepadEvent, GamepadFrame};
    use std::time::{Duration, Instant};
    let secs: u64 = args
        .iter()
        .skip_while(|a| *a != "--seconds")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    // `--index N` creates pad `pf_pad_N` (default 0) — use a spare index (e.g. 1) to test
    // alongside a running host that already holds pad 0. `--ds4` drives the DualShock 4
    // backend instead of the DualSense one.
    let idx: u8 = args
        .iter()
        .skip_while(|a| *a != "--index")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let ds4 = args.iter().any(|a| a == "--ds4");
    let xbox = args.iter().any(|a| a == "--xbox");
    // `--edge` drives the DualSense Edge backend (device_type 2) and additionally holds
    // the R4/L4 paddles on the pressed beats, so a HID read shows the Edge bits in
    // report byte 10 (0x80|0x40) next to Cross. `--deck` drives the Steam Deck backend
    // (device_type 3, the MI_02-promoted identity) — watch Steam claim it live.
    let edge = args.iter().any(|a| a == "--edge");
    let deck = args.iter().any(|a| a == "--deck");
    let extra_buttons: u32 = if edge || deck {
        punktfunk_core::input::gamepad::BTN_PADDLE1 | punktfunk_core::input::gamepad::BTN_PADDLE2
    } else {
        0
    };
    // Same drive loop for either backend (identical method surface): Arrival creates the pad,
    // State pushes a cycling report, pump surfaces a game's rumble/lightbar feedback.
    macro_rules! drive {
        ($mgr:expr, $label:expr) => {{
            let mut mgr = $mgr;
            mgr.handle(&GamepadEvent::Arrival {
                index: idx,
                kind: 2,
                capabilities: 0,
            });
            println!(
                "virtual {} up — cycling Cross + sweeping the left stick for {secs}s. Watch \
                 it in joy.cpl / Steam / a game; any feedback the game sends prints below.",
                $label
            );
            let deadline = Instant::now() + Duration::from_secs(secs);
            let (mut i, mut last) = (0i32, Instant::now());
            while Instant::now() < deadline {
                mgr.pump(
                    |pad, lo, hi| println!("  rumble from game: pad={pad} low={lo} high={hi}"),
                    |o| println!("  hid output from game: {o:?}"),
                );
                if last.elapsed() >= Duration::from_millis(400) {
                    last = Instant::now();
                    i += 1;
                    let buttons = if i % 2 == 0 {
                        punktfunk_core::input::gamepad::BTN_A | extra_buttons // Cross (+ Edge paddles)
                    } else {
                        0
                    };
                    let lx = (((i % 64) - 32) * 1024) as i16; // sweep left stick X
                    mgr.handle(&GamepadEvent::State(GamepadFrame {
                        index: idx as i16,
                        active_mask: 1 << idx,
                        buttons,
                        left_trigger: 0,
                        right_trigger: 0,
                        ls_x: lx,
                        ls_y: 0,
                        rs_x: 0,
                        rs_y: 0,
                    }));
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        }};
    }
    if xbox {
        // Xbox 360 via the XUSB companion: a different surface (handle + pump_rumble, no
        // HID-output plane), so drive it inline rather than via the macro.
        let mut mgr = crate::inject::gamepad::GamepadManager::new();
        mgr.handle(&GamepadEvent::Arrival {
            index: idx,
            kind: 1,
            capabilities: 0,
        });
        println!(
            "virtual Xbox 360 (XUSB) up — sweeping LS + toggling A for {secs}s. Check with \
             an XInput game or xinputtest.exe."
        );
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut t = 0i32;
        while Instant::now() < deadline {
            mgr.pump_rumble(|pad, lo, hi| {
                println!("  rumble from game: pad={pad} low={lo} high={hi}")
            });
            t += 1;
            let lx = (((t % 200) - 100) * 327).clamp(-32768, 32767) as i16; // sweep ±32700
            let buttons = if (t / 67) % 2 == 0 {
                punktfunk_core::input::gamepad::BTN_A
            } else {
                0
            };
            mgr.handle(&GamepadEvent::State(GamepadFrame {
                index: idx as i16,
                active_mask: 1 << idx,
                buttons,
                left_trigger: 0,
                right_trigger: 0,
                ls_x: lx,
                ls_y: 0,
                rs_x: 0,
                rs_y: 0,
            }));
            std::thread::sleep(Duration::from_millis(15));
        }
    } else if ds4 {
        drive!(
            crate::inject::dualshock4_windows::DualShock4WindowsManager::new(),
            "DualShock 4"
        );
    } else if edge {
        drive!(
            crate::inject::dualsense_edge_windows::DualSenseEdgeWindowsManager::new(),
            "DualSense Edge"
        );
    } else if deck {
        drive!(
            crate::inject::steam_deck_windows::SteamDeckWindowsManager::new(),
            "Steam Deck"
        );
    } else {
        drive!(
            crate::inject::dualsense_windows::DualSenseWindowsManager::new(),
            "DualSense"
        );
    }
    println!("dualsense-windows-test: done (devnode removed)");
    Ok(())
}

/// Mirror a physical monitor and pull frames from it — the on-glass gate for per-monitor capture
/// (`design/per-monitor-portal-capture.md` P2/P3), without needing a client to connect.
///
/// Opens the display backend exactly as a session would (so a `PUNKTFUNK_CAPTURE_MONITOR` pin
/// routes to the mirror backend), attaches a capturer to whatever PipeWire node comes back, and
/// reports the frames it actually receives. What it proves that a unit test cannot: the compositor
/// accepted the record request for a NAMED head, and that head is producing pixels at its own size.
///
/// `--monitor <CONNECTOR>` pins for this run (else `PUNKTFUNK_CAPTURE_MONITOR`); `--seconds N`.
#[cfg(target_os = "linux")]
pub fn mirror_test(args: &[String]) -> Result<()> {
    use std::time::{Duration, Instant};
    let arg = |name: &str| {
        args.iter()
            .skip_while(|a| a.as_str() != name)
            .nth(1)
            .cloned()
    };
    let secs: u64 = arg("--seconds").and_then(|s| s.parse().ok()).unwrap_or(5);
    // `--monitor` cannot work by setting PUNKTFUNK_CAPTURE_MONITOR here: pf_host_config parses the
    // environment ONCE and startup already read it, so this process would still see the old
    // snapshot. An explicit connector therefore goes through `open_mirror` below; only the unset
    // case falls back to the pin (and to `open`, which is the production routing).
    let explicit = arg("--monitor");
    let want = explicit
        .clone()
        .or_else(|| pf_host_config::config().capture_monitor.clone())
        .context(
            "no monitor named — pass --monitor <CONNECTOR> or set PUNKTFUNK_CAPTURE_MONITOR",
        )?;

    let compositor = crate::vdisplay::detect()?;
    let monitors = crate::vdisplay::monitors::list(compositor)?;
    let target = crate::vdisplay::monitors::resolve(&monitors, &want)?;
    println!(
        "mirror-test: {compositor:?} {} ({}) at +{},+{}",
        target.connector,
        target.mode_label(),
        target.x,
        target.y
    );

    // No `--monitor` ⇒ exercise the PRODUCTION routing (`open` consulting the pin), which is the
    // more valuable path to prove; an explicit connector takes the direct opener.
    let mut vd = match &explicit {
        Some(connector) => crate::vdisplay::open_mirror(compositor, connector)?,
        None => crate::vdisplay::open(compositor)?,
    };
    // The mode is ignored by the mirror backend (a panel runs at the mode its owner set); pass the
    // head's own so this also behaves if the pin is ever unset mid-test.
    let mode = crate::vdisplay::Mode {
        width: target.width,
        height: target.height,
        refresh_hz: 60,
    };
    let vout = vd.create(mode).context("open the mirror display")?;
    println!(
        "mirror-test: node_id={} preferred={:?} ownership={:?}",
        vout.node_id, vout.preferred_mode, vout.ownership
    );

    // Default to the GPU (dmabuf zero-copy) path a real session uses; `--cpu` forces the mmap
    // path, which is worth having as a switch — the two negotiate different PipeWire buffer types.
    let gpu = !args.iter().any(|a| a == "--cpu");
    let fmt = pf_frame::OutputFormat::resolve(false, gpu);
    println!(
        "mirror-test: capture path = {}",
        if gpu { "gpu/dmabuf" } else { "cpu/mmap" }
    );
    let mut cap = crate::capture::capture_virtual_output(
        vout,
        fmt,
        crate::session_plan::CaptureBackend::resolve(),
    )
    .context("attach a capturer to the mirrored monitor")?;
    cap.set_active(true);

    let deadline = Instant::now() + Duration::from_secs(secs);
    let (mut frames, mut first) = (0u32, None);
    let mut idle = 0u32;
    let mut dims = (0u32, 0u32);
    while Instant::now() < deadline {
        match cap.next_frame_within(Duration::from_secs(5)) {
            Ok(f) => {
                if first.is_none() {
                    first = Some(Instant::now());
                    println!(
                        "mirror-test: FIRST FRAME {}x{} {:?}",
                        f.width, f.height, f.format
                    );
                }
                dims = (f.width, f.height);
                frames += 1;
            }
            // A timeout is NOT fatal here: compositor screencast is damage-driven, so a static
            // desktop legitimately produces nothing for seconds at a time (the host's own capture
            // diag logs `new_fps=0` for virtual outputs on an idle desktop for the same reason).
            // Keep waiting until the deadline instead of ending the measurement on the first gap.
            Err(e) => {
                idle += 1;
                if idle == 1 {
                    println!("mirror-test: (idle — no damage yet: {e:#})");
                }
            }
        }
    }
    match first {
        Some(_) => println!(
            "mirror-test: OK — {frames} frames in {secs}s at {}x{} ({:.1} fps over the whole run, \
             {idle} idle gaps). Compositor capture is damage-driven: a static desktop produces \
             nothing, so judge this by whether frames track what is happening on screen.",
            dims.0,
            dims.1,
            frames as f64 / secs as f64
        ),
        None => {
            anyhow::bail!("no frames arrived in {secs}s — the cast started but produced nothing")
        }
    }
    Ok(())
}
