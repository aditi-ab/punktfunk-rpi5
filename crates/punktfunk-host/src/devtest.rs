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
        // `lt`/`rt` are structurally always zero here — a Switch Pro has no trigger motors —
        // but this harness reads the shared `PadFeedback`, so it prints all four levels.
        if let Some((low, high, lt, rt)) = fb.rumble {
            println!("  rumble from kernel/game: low={low} high={high} lt={lt} rt={rt}");
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

/// Windows CHANNEL-PROOF PROBE: settle, on a real box, which HID IOCTL hidclass forwards to a UMDF
/// HID minidriver — the one assumption in the pad channel's v3 delivery gate that could not be
/// settled by reading. Spins up a throwaway `pf_mouse_probe` devnode at pad index 9 (so it is safe
/// to run beside a live host), asks it for its channel proof over both HID paths, and prints which
/// answered. Needs the CURRENT drivers installed: `punktfunk-host.exe driver install --gamepad`.
#[cfg(target_os = "windows")]
pub fn channel_proof_probe(_args: &[String]) -> Result<()> {
    crate::inject::mouse_windows::channel_proof_probe()
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
    // `--xboxhid` drives the HID Xbox backend (device-type 4) instead of `--xbox`'s XUSB companion.
    let xboxhid = args.iter().any(|a| a == "--xboxhid");
    // The other two HID Xbox identities (device-types 5 and 6). Same backend, same report
    // descriptor — only VID/PID, product string and hardware id differ — so these legs exist for
    // exactly one question each: does Windows PROMOTE that PID the way it promotes `0B13`?
    // `02FD` in particular has no stage-2 `HID\…&IG_00` line in Microsoft's `xinputhid.inf`, so
    // it is the one worth watching. Check for the `IG_00` token, the XUSB interface, an XInput
    // slot and rumble, exactly as the `--xboxhid` run did.
    let xboxones = args.iter().any(|a| a == "--xboxones");
    let xboxelite = args.iter().any(|a| a == "--xboxelite");
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
                audio_caps: 0,
            });
            // 🛑 Never announce a pad that was not built. The arrival above only ASKS for one; a
            // failed create logs an ERROR and leaves the slot empty, and this harness would then
            // cheerfully print "virtual X up" and stream frames into nothing for `secs` seconds.
            // Every probe the operator runs next (joy.cpl, XInputGetState, a WGI enumeration) still
            // finds a device on this index — the one the OTHER process owns — so the run produces a
            // plausible, wrong measurement instead of a failure. That happened on `.173`
            // (2026-08-09): the host service held pad 0, the create was denied, and a frozen XInput
            // packet count off the incumbent pad was read as a result. A harness that cannot build
            // its own device has nothing to measure, so stop.
            if mgr.live_pads() == 0 {
                anyhow::bail!(
                    "no virtual {} was created at index {idx} — see the ERROR above for the \
                     cause. NOT measuring: any device answering on this index belongs to another \
                     process (a live session's pad), and reading it would look like a result.",
                    $label
                );
            }
            println!(
                "virtual {} up — cycling Cross + sweeping the left stick for {secs}s. Watch \
                 it in joy.cpl / Steam / a game; any feedback the game sends prints below.",
                $label
            );
            let deadline = Instant::now() + Duration::from_secs(secs);
            let (mut i, mut last) = (0i32, Instant::now());
            while Instant::now() < deadline {
                mgr.pump(
                    |pad, lo, hi, lt, rt| println!(
                        "  rumble from game: pad={pad} low={lo} high={hi} lt={lt} rt={rt}"
                    ),
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
                    // 🛑 Sweep EVERY analogue axis, each on its own phase, and ramp both triggers.
                    //
                    // This used to drive LS-X alone and leave the other five at zero, which makes
                    // the harness unable to tell "this axis is not mapped" from "nothing is driving
                    // it" — the two look identical in any consumer. That is exactly how a DEAD
                    // RIGHT STICK survived every bench measurement of the Windows HID Xbox pad and
                    // was found only on glass (2026-08-09): `XInputGetState` read `RX [0..0]` and it
                    // was written off as "the devtest doesn't move it", which was true and useless.
                    // Distinct phases mean one run tells you which axes arrive AND that they are not
                    // crosstalking onto each other's bytes.
                    let phase = |off: i32| ((((i + off) % 64) - 32) * 1024) as i16;
                    let trig = ((i % 32) * 8).clamp(0, 255) as u8;
                    mgr.handle(&GamepadEvent::State(GamepadFrame {
                        index: idx as i16,
                        active_mask: 1 << idx,
                        buttons,
                        left_trigger: trig,
                        right_trigger: 255 - trig,
                        ls_x: phase(0),
                        ls_y: phase(16),
                        rs_x: phase(32),
                        rs_y: phase(48),
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
            audio_caps: 0,
        });
        // Same guard as the `drive!` macro's — see the long note there.
        if mgr.live_pads() == 0 {
            anyhow::bail!(
                "no virtual Xbox 360 (XUSB) was created at index {idx} — see the ERROR above. NOT \
                 measuring: a device answering on this index belongs to another process."
            );
        }
        println!(
            "virtual Xbox 360 (XUSB) up — sweeping LS + toggling A for {secs}s. Check with \
             an XInput game or xinputtest.exe."
        );
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut t = 0i32;
        while Instant::now() < deadline {
            // `lt`/`rt` are structurally always zero on XUSB (see `pump_rumble`); printed so
            // the harness output is comparable line-for-line with the HID Xbox backend's.
            mgr.pump_rumble(|pad, lo, hi, lt, rt| {
                println!("  rumble from game: pad={pad} low={lo} high={hi} lt={lt} rt={rt}")
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
    } else if xboxhid {
        // The HID Xbox pad (device-type 4) — the SHIPPING SwDeviceCreate identity, not a devgen
        // node. That distinction is the whole point of this leg: a devgen devnode carries no USB
        // hardware ids, so its HID child comes up `HID\VID_045E&UP:0001_U:0005` with no PID token,
        // and the question this exists to answer — does Windows promote our pad to an Xbox-profile
        // device (an `IG_` token, XInput, WGI `Gamepad`) — turns on exactly that PID being present.
        drive!(
            crate::inject::xbox_windows::XboxWindowsManager::new(),
            "Xbox Wireless Controller (HID)"
        );
    } else if xboxones {
        drive!(
            crate::inject::xbox_windows::XboxWindowsManager::with_backend(
                crate::inject::xbox_windows::XboxWinProto::one_s()
            ),
            "Xbox One S Controller (HID, 045E:02FD)"
        );
    } else if xboxelite {
        drive!(
            crate::inject::xbox_windows::XboxWindowsManager::with_backend(
                crate::inject::xbox_windows::XboxWinProto::elite()
            ),
            "Xbox Elite Wireless Controller Series 2 (HID, 045E:0B22)"
        );
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

/// Windows: pad-audio endpoint provisioning — `pad-endpoint ensure|remove|status [--index N]`.
/// `ensure` runs the idempotent startup path (reuse-or-create the devnode, bind the Steam
/// Streaming Speakers driver, stamp the DualSense identity + 4ch/48k formats, report whether
/// the stamps are SERVED); `status` prints the devnode/endpoint and per-stamp stored vs served
/// state without changing anything; `remove` deletes the devnode via pnputil — the escape
/// hatch only, endpoints are persistent by design. Stamping needs SYSTEM (the MMDevices ACL);
/// run `ensure` under the service account or PsExec when the property-store route is denied.
/// Windows: the audio-substrate toolbox (`windows-audio-endpoints-and-vbcable.md`) —
/// `audio-probe ssm|sink|sss-primary|mint|plan|cleanup [--keep]`. The S1–S3 spikes (`ssm` =
/// the decision gate: mint a second Steam Streaming Microphone devnode and prove
/// render→capture end to end; `sink` parks the default on a minted Speakers instance and
/// loopback-measures it; `sss-primary` re-measures the primary Speakers' loopback), plus the
/// product paths: `mint` runs the minted-endpoint provider synchronously and `plan` prints
/// one real wiring pass with its readiness verdict.
#[cfg(target_os = "windows")]
pub fn audio_probe(args: &[String]) -> Result<()> {
    crate::audio::audio_probe::run(args)
}

#[cfg(target_os = "windows")]
pub fn pad_endpoint(args: &[String]) -> Result<()> {
    use crate::audio::pad_endpoint as pe;
    let idx: u8 = args
        .iter()
        .skip_while(|a| *a != "--index")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // `--endpoint <id>` drives ANY render endpoint, not just a provisioned pad one. It is the
    // discriminator between "this process cannot activate anything" and "our endpoint is broken":
    // aim the same binary at a known-good endpoint and see whether it succeeds there.
    let endpoint_override: Option<String> = args
        .iter()
        .skip_while(|a| *a != "--endpoint")
        .nth(1)
        .cloned();
    match args.get(1).map(String::as_str) {
        Some("ensure") => {
            let p = pe::ensure(idx)?;
            println!(
                "pad-endpoint ensure: pad {} devnode {} endpoint {} needs_aeb_kick={}",
                p.pad_index, p.device_instance, p.endpoint_id, p.needs_aeb_kick
            );
            Ok(())
        }
        Some("remove") => match pe::find(idx)? {
            Some(p) => {
                pe::remove(&p);
                println!(
                    "pad-endpoint remove: requested removal of {}",
                    p.device_instance
                );
                Ok(())
            }
            None => {
                println!("pad-endpoint remove: no pad-audio devnode for index {idx}");
                Ok(())
            }
        },
        // `punktfunk-host pad-endpoint <n> tone [seconds] [hz]` — drive the endpoint directly so
        // the whole pad-audio chain can be exercised without a game. Without this, every attempt
        // costs a game launch and a failure does not say which link broke.
        Some("tone") => {
            let secs: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
            let hz: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(60.0);
            let endpoint_id = match endpoint_override {
                Some(id) => id,
                None => {
                    // `find` (a system lookup), NOT `endpoint_for` (the service's in-process
                    // cache): this runs as a separate CLI process and has no cache of its own.
                    let Some(ep) = pe::find(idx)? else {
                        println!(
                            "pad-endpoint tone: no pad-audio devnode for pad {idx} — run \
                             `ensure` first"
                        );
                        return Ok(());
                    };
                    if ep.endpoint_id.is_empty() {
                        println!("pad-endpoint tone: pad {idx} has no endpoint id yet");
                        return Ok(());
                    }
                    ep.endpoint_id
                }
            };
            // `--pair front` drives the pad's SPEAKER instead of the voice coils — the only way to
            // exercise the speaker kind without a game that renders one.
            let pair = args
                .iter()
                .skip_while(|a| *a != "--pair")
                .nth(1)
                .map_or(pe::TonePair::Back, |s| pe::TonePair::parse(s));
            println!(
                "pad-endpoint tone: {hz} Hz into the {} of {endpoint_id} for {secs}s",
                pair.label()
            );
            pe::render_test_tone(&endpoint_id, secs, hz, pair)?;
            println!(
                "pad-endpoint tone: done. A connected client with pad audio enabled should have \
                 buzzed; the host log shows whether the gate opened."
            );
            Ok(())
        }
        // `punktfunk-host pad-endpoint capture [seconds]` — the receiving half of `tone`. Run
        // both at once to exercise render -> engine -> loopback -> pair routing with no game and
        // no client attached.
        Some("capture") => {
            let secs: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
            let endpoint_id = match endpoint_override {
                Some(id) => id,
                None => match pe::find(idx)? {
                    Some(ep) if !ep.endpoint_id.is_empty() => ep.endpoint_id,
                    _ => {
                        println!("pad-endpoint capture: pad {idx} has no endpoint — run `ensure`");
                        return Ok(());
                    }
                },
            };
            println!("pad-endpoint capture: listening on {endpoint_id} for {secs}s");
            pe::capture_probe(&endpoint_id, secs)
        }
        Some("status") => pe::print_status(idx),
        _ => anyhow::bail!("usage: punktfunk-host pad-endpoint <ensure|remove|status> [--index N]"),
    }
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
        .or_else(crate::vdisplay::capture_monitor)
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
        compositor == crate::vdisplay::Compositor::Kwin,
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

/// Aim absolute input at a named monitor and prove where it landed — the on-glass gate for the
/// input-region ladder (`design/per-monitor-portal-capture.md` §7.2), without needing a client.
///
/// The ladder exists for one case a unit test can only simulate: **two heads of the same size**,
/// where matching a libei region by the streamed mode is a coin flip and the pointer silently ends
/// up on the wrong screen. This drives the real thing — the compositor's own EIS regions, the real
/// anchor, the real resolver — and prints the region absolute coordinates actually mapped into, so
/// "it went to the right monitor" is something you read rather than infer.
///
/// `--monitor <CONNECTOR>` anchors at that head's origin (default: the `PUNKTFUNK_CAPTURE_MONITOR` /
/// policy pin); `--none` deliberately runs UNANCHORED, which is the A/B that makes the anchored run
/// mean something on a same-size pair. It then walks the corners and centre of a `--width`×`--height`
/// client surface so an observer can watch the pointer.
///
/// Read the answer from the log line `libei: absolute input maps into this output`.
#[cfg(target_os = "linux")]
pub fn anchor_test(args: &[String]) -> Result<()> {
    use punktfunk_core::input::{InputEvent, InputKind};
    use std::time::Duration;
    let arg = |name: &str| {
        args.iter()
            .skip_while(|a| a.as_str() != name)
            .nth(1)
            .cloned()
    };
    let unanchored = args.iter().any(|a| a == "--none");
    let w: u32 = arg("--width").and_then(|s| s.parse().ok()).unwrap_or(1920);
    let h: u32 = arg("--height").and_then(|s| s.parse().ok()).unwrap_or(1080);

    let compositor = crate::vdisplay::detect()?;
    let monitors = crate::vdisplay::monitors::list(compositor)?;
    println!(
        "anchor-test: {compositor:?} has {} monitor(s):",
        monitors.len()
    );
    for m in &monitors {
        println!(
            "  {:<12} {:>13} at +{},+{}",
            m.connector,
            m.mode_label(),
            m.x,
            m.y
        );
    }
    // Two heads at the same size is the case the ladder exists for; say so when the rig is right,
    // and say so when it is NOT — a green run on a single-head box proves nothing about it.
    let same_size = monitors.iter().enumerate().any(|(i, a)| {
        monitors
            .iter()
            .skip(i + 1)
            .any(|b| a.width == b.width && a.height == b.height)
    });
    println!(
        "anchor-test: two same-size heads present: {} {}",
        same_size,
        if same_size {
            "— this run exercises the case the ladder exists for"
        } else {
            "— WEAK RIG: size matching would have picked correctly anyway"
        }
    );

    if unanchored {
        crate::inject::set_absolute_anchor(None);
        println!("anchor-test: UNANCHORED (--none) — the size/first rungs decide");
    } else {
        let want = arg("--monitor")
            .or_else(crate::vdisplay::capture_monitor)
            .context("no monitor named — pass --monitor <CONNECTOR>, or --none for the A/B")?;
        let m = crate::vdisplay::monitors::resolve(&monitors, &want)?;
        crate::inject::set_absolute_anchor(Some(crate::inject::AbsoluteAnchor {
            origin: Some((m.x, m.y)),
            mapping_id: None,
        }));
        println!(
            "anchor-test: anchored at {} +{},+{} ({})",
            m.connector,
            m.x,
            m.y,
            m.mode_label()
        );
    }

    let backend = crate::inject::default_backend();
    if backend != crate::inject::Backend::Libei {
        // The ladder is libei's; on any other backend this command would report nothing about it.
        // Say so rather than emitting a green run that means nothing (sway injects via WlrVirtual,
        // which is why the sway box cannot serve as this rig — set PUNKTFUNK_INPUT_BACKEND=libei on
        // a compositor that speaks EI).
        anyhow::bail!(
            "input backend is {backend:?}, not libei — the absolute-region ladder only exists on \
             the libei backend; set PUNKTFUNK_INPUT_BACKEND=libei"
        );
    }
    let mut inj = crate::inject::open(backend)?;
    // libei establishes its portal/EIS session + device resume asynchronously; events before then
    // are dropped (and it is the resume that publishes the regions we are testing).
    std::thread::sleep(Duration::from_secs(4));

    let flags = (w << 16) | (h & 0xffff);
    let pts = [
        (w as i32 / 2, h as i32 / 2),
        (60, 60),
        (w as i32 - 60, 60),
        (w as i32 - 60, h as i32 - 60),
        (60, h as i32 - 60),
        (w as i32 / 2, h as i32 / 2),
    ];
    println!("anchor-test: walking {w}x{h} — centre, four corners, centre (1s apart)");
    for (x, y) in pts {
        let e = InputEvent {
            kind: InputKind::MouseMoveAbs,
            _pad: [0; 3],
            code: 0,
            x,
            y,
            flags,
        };
        if let Err(err) = inj.inject(&e) {
            tracing::warn!(error = %format!("{err:#}"), "anchor-test: inject failed");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    println!(
        "anchor-test: done — read the `libei: absolute input maps into this output` line above \
         for the region that was chosen"
    );
    Ok(())
}
