//! CLI arms that exercise a host subsystem without a streaming client.
//!
//! Linux: UHID DualSense / Switch Pro, libei/wlr input, pad-sink and usbip audio,
//! per-monitor mirror, libei absolute-input ladder. Windows: UMDF DualSense-family,
//! Steam Deck spike, pad-audio endpoints. Each fn is the full `punktfunk-host`
//! subcommand; `main.rs` only forwards. Flags live on the fn.

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;

/// Scripted stylus through [`PenTracker`](punktfunk_core::quic::PenTracker) → the "Punktfunk Pen"
/// uinput tablet. No client.
///
/// Hover in, tip down, sine stroke with pressure ramp + tilt, tip up, hover out.
/// Watch a pressure brush, or `sudo libinput debug-events` (`TABLET_TOOL_PROXIMITY`
/// / `TIP` / `AXIS`). `design/pen-tablet-input.md`.
#[cfg(target_os = "linux")]
pub fn pen_test() -> Result<()> {
    use punktfunk_core::quic::{
        PenBatch, PenSample, PenTracker, PenTransition, PEN_IN_RANGE, PEN_TOUCHING,
    };
    use std::time::Duration;

    let mut dev = crate::inject::pen::VirtualPen::create()?;
    let mut tracker = PenTracker::default();
    let mut out: Vec<PenTransition> = Vec::new();
    // 2 s: compositor enumerates the new evdev node; events before that are dropped.
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
                // 6553..65535 (10 %→100 %): a pressure brush must visibly widen.
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

/// Scripted mouse + keyboard through the session input backend (libei / wlr). No client.
#[cfg(target_os = "linux")]
pub fn input_test() -> Result<()> {
    use punktfunk_core::input::{InputEvent, InputKind};
    use std::time::Duration;

    let backend = crate::inject::default_backend();
    tracing::info!(?backend, "input-test: opening injector");
    let mut inj = crate::inject::open(backend)?;
    // 4 s: libei portal/EIS session + device resume; events before that are dropped.
    std::thread::sleep(Duration::from_secs(4));

    let ev = |kind, code, x, y| InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y,
        flags: 0,
    };
    // `PUNKTFUNK_INPUT_TEST_ABS=WxH`: MouseMoveAbs (touch → abs). `xdotool getmouselocation` should jump.
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

/// Virtual DualSense via UHID: Cross, left-stick sweep, print kernel HID output. No session.
///
/// `evtest`, `/dev/input/by-id/*Punktfunk*`, `wpctl status`. `--edge` is 054C:0DF2 and
/// cycles the four back paddles (`BTN_TRIGGER_HAPPY1..4` on kernel ≥ 7.2; older kernels:
/// bind + hidraw byte 10).
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
    // 800 ms: hid-playstation GET_REPORT init; input nodes appear after that.
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
                // One paddle per beat so all four Edge slots show in evtest.
                buttons |= punktfunk_core::input::gamepad::BTN_PADDLE1 << (i % 4);
            }
            let lx = (((i % 64) - 32) * 1024) as i16;
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

/// Mint a DualSense-shaped PipeWire graph (`audio::pad_sink`) and capture the mix. No client.
///
/// Three nodes: mono `Speaker__sink`, positioned-quad `SpeakerHaptic__sink`, hidden AUX
/// parent. `pactl list sinks`; `pw-play --target <node.name>`. `--pad N`, `--edge`,
/// `--seconds N` (default 30).
#[cfg(target_os = "linux")]
pub fn pad_sink_test(args: &[String]) -> Result<()> {
    use crate::audio::AudioCapturer as _;
    use std::time::{Duration, Instant};
    let secs: u64 = args
        .iter()
        .skip_while(|a| *a != "--seconds")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let pad: u8 = args
        .iter()
        .skip_while(|a| *a != "--pad")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let edge = args.iter().any(|a| a == "--edge");
    let mut cap = crate::audio::pad_sink::PadSinkCapturer::open(pad, edge)
        .context("mint pad-audio sink (is PipeWire running in this session?)")?;
    println!(
        "pad nodes minted (the split a real DualSense presents):\n  \
         speaker sink  = {}    (mono — GE-Proton's is_dualsense_speaker_sink target)\n  \
         haptic sink   = {}    (4ch POSITIONED FL,FR,RL,RR — the public quad a real pad shows)\n  \
         parent        = {}    (4ch AUX0..AUX3, hidden — what GE opens as pipewire:NODE=…)\n  \
         inspect: pactl list sinks | grep -A25 Speaker\n  \
         drive the coils via the POSITIONED sink (what a real pad's writers use):\n    \
         pw-play --target '{}' --channel-map 'front-left,front-right,rear-left,rear-right' <48k-file>\n  \
         drive the coils via the AUX parent (GE's own leg):\n    \
         pw-play --target '{}' --channel-map 'AUX0,AUX1,AUX2,AUX3' <48k-file>\n  \
         (a POSITIONED wav aimed at the AUX PARENT still folds into the speaker pair — that is \
         why the positioned sink exists)\nCapturing for {secs}s…",
        cap.node_name,
        cap.haptic_name,
        if cap.split_name.is_empty() {
            "(suppressed)"
        } else {
            cap.split_name.as_str()
        },
        cap.haptic_name,
        cap.split_name,
    );
    let deadline = Instant::now() + Duration::from_secs(secs);
    let (mut chunks, mut samples) = (0u64, 0u64);
    // split_quad: ch0/1 speaker, ch2/3 coils. A remix can zero one pair while a global peak looks fine.
    let (mut peak_spk, mut peak_coil) = (0f32, 0f32);
    let mut last_report = Instant::now();
    while Instant::now() < deadline {
        let c = cap.next_chunk().context("pad sink capture")?;
        if !c.is_empty() {
            chunks += 1;
            samples += c.len() as u64;
            for f in c.chunks_exact(4) {
                peak_spk = peak_spk.max(f[0].abs()).max(f[1].abs());
                peak_coil = peak_coil.max(f[2].abs()).max(f[3].abs());
            }
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            println!(
                "  chunks={chunks} samples={samples} (~{:.1}ms of 4ch audio) \
                 peak_speaker={peak_spk:.4} peak_coils={peak_coil:.4}",
                samples as f64 / (4.0 * 48.0)
            );
            (chunks, samples, peak_spk, peak_coil) = (0, 0, 0.0, 0.0);
        }
    }
    println!("pad-sink-test: done");
    Ok(())
}

/// usbip DualSense: real USB + UAC card, capture from the isochronous endpoint. No client.
///
/// Proves what UHID cannot: `vhci_hcd` + `hid-playstation` bind, `snd-usb-audio` ALSA card
/// (GE-Proton `snd_card_next`; a minted PipeWire node is never that), USB parent so wine
/// walks HID → `usb_device` and gets a non-null ContainerId, then split_quad samples.
/// Ignores `PUNKTFUNK_DUALSENSE_USBIP` — this command is the opt-in. `--pad N`, `--seconds N`.
#[cfg(target_os = "linux")]
pub fn pad_usbip_test(args: &[String]) -> Result<()> {
    use crate::audio::AudioCapturer as _;
    use std::time::{Duration, Instant};
    let arg = |name: &str, default: u64| -> u64 {
        args.iter()
            .skip_while(|a| *a != name)
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    };
    let secs = arg("--seconds", 30);
    let pad = arg("--pad", 0) as u8;

    let _pad = pf_inject::dualsense_usbip::DualSenseUsbip::open(pad).context(
        "attach the usbip DualSense (is vhci_hcd loaded, and is \
         /sys/devices/platform/vhci_hcd.0/attach writable by the `punktfunk` group?)",
    )?;
    // 1.5 s: vhci enumerate + hid/snd bind + PipeWire; report before that looks like a miss.
    std::thread::sleep(Duration::from_millis(1500));

    match pf_inject::dualsense_usbip::find_usb_topology() {
        Some(t) => println!(
            "usb device attached:\n  \
             sysfs         = {}\n  \
             busnum/devnum = {}/{}   (with the vendor/product pair, these are exactly the fields \
             wine packs into the ContainerId — the pad's HID device and its audio sink must both \
             resolve to THIS node)\n  \
             check the sink agrees:\n    \
             pactl list sinks | grep -E 'Name:|sysfs'\n  \
             (`sysfs.path` on the sink is what winepulse prefixes with /sys and walks up; with a \
             real card PipeWire fills it in itself)",
            t.sysfs_path.display(),
            t.busnum,
            t.devnum,
        ),
        None => println!(
            "⚠ no 054c:0ce6 usb device found under vhci_hcd — the attach reported success but the \
             kernel did not enumerate it. Check `dmesg | tail -40`."
        ),
    }
    match std::fs::read_to_string("/proc/asound/cards") {
        Ok(cards) if cards.contains("DualSense") => println!(
            "alsa card present (GE-Proton's snd_card_next scan can see this):\n{}",
            cards.trim_end()
        ),
        Ok(cards) => println!(
            "⚠ no DualSense ALSA card — snd-usb-audio did NOT bind the audio function, so \
             GE-Proton's raw-ALSA haptic leg stays blind. /proc/asound/cards:\n{}\n  \
             check `dmesg | grep -i 'usb\\|snd' | tail -30`",
            cards.trim_end()
        ),
        Err(e) => println!("⚠ could not read /proc/asound/cards: {e}"),
    }
    println!(
        "drive it (either route converges on the pad's isochronous endpoint):\n  \
         via PipeWire:  pw-play --target <the pad's SpeakerHaptic sink> \
         --channel-map 'front-left,front-right,rear-left,rear-right' <48k-file>\n  \
         raw ALSA:      aplay -D plughw:CARD=Controller -f S16_LE -r 48000 -c 4 <48k-file>\n\
         Capturing for {secs}s…"
    );

    let mut cap = crate::audio::pad_usb::PadUsbCapturer::open(pad)
        .context("claim the usbip pad's audio stream")?;
    let deadline = Instant::now() + Duration::from_secs(secs);
    let (mut chunks, mut samples) = (0u64, 0u64);
    // split_quad: ch0/1 speaker, ch2/3 coils. A UAC channel-order slip zeros one pair while a global peak looks fine.
    let (mut peak_spk, mut peak_coil) = (0f32, 0f32);
    let mut last_report = Instant::now();
    while Instant::now() < deadline {
        let c = cap.next_chunk().context("usb pad capture")?;
        if !c.is_empty() {
            chunks += 1;
            samples += c.len() as u64;
            for f in c.chunks_exact(4) {
                peak_spk = peak_spk.max(f[0].abs()).max(f[1].abs());
                peak_coil = peak_coil.max(f[2].abs()).max(f[3].abs());
            }
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            println!(
                "  chunks={chunks} samples={samples} (~{:.1}ms of 4ch audio) \
                 peak_speaker={peak_spk:.4} peak_coils={peak_coil:.4}",
                samples as f64 / (4.0 * 48.0)
            );
            (chunks, samples, peak_spk, peak_coil) = (0, 0, 0.0, 0.0);
        }
    }
    println!("pad-usbip-test: done");
    Ok(())
}

/// Virtual Switch Pro via UHID: hid-nintendo probe, then A/B + left-stick sweep. No session.
///
/// `evtest`, `dmesg | grep nintendo`, SDL "Nintendo Switch Pro Controller". A/B are
/// positionally swapped.
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
    // 2.5 s: every hid-nintendo probe step blocks until the reply; stream 0x30 like hardware.
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
        // Switch Pro has no trigger motors; `PadFeedback` still carries four rumble levels.
        if let Some((low, high, lt, rt)) = fb.rumble {
            println!("  rumble from kernel/game: low={low} high={high} lt={lt} rt={rt}");
        }
        for o in fb.hidout {
            println!("  hid output from kernel/game: {o:?}");
        }
        // 15 ms: real Pro report rate; also feeds hid-nintendo's post-probe rate limiter.
        if last_write.elapsed() >= Duration::from_millis(15) {
            last_write = Instant::now();
            i += 1;
            let step = i / 20; // ~300 ms at 15 ms/report
            let buttons = if step % 2 == 0 {
                punktfunk_core::input::gamepad::BTN_A
            } else {
                punktfunk_core::input::gamepad::BTN_B
            };
            let lx = (((i % 64) - 32) * 1024) as i16;
            let st = SwitchState::from_gamepad(buttons, lx, 0, 0, 0, 0, 0);
            pad.write_state(&st).context("write Switch Pro report")?;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    println!("switchpro-test: done");
    Ok(())
}

/// Hold a software-devnode HID Steam Deck (28DE:1205, device_type 3) and watch Steam Input
/// promote it. Signed driver + Steam running. `--seconds N` (default 120).
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

/// Hold the pf-mouse virtual HID pointer and sweep the cursor via HID reports.
///
/// Stack: devnode → INF → mshidumdf → mouhid → win32k. A resident pointer makes
/// `SM_MOUSEPRESENT` true so DWM composites the cursor with no dongle. Stop the host
/// service first — it owns the mailbox. `--seconds N` (default 30).
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

/// Probe which HID IOCTL hidclass forwards to a UMDF HID minidriver.
///
/// Throwaway `pf_mouse_probe` at pad index 9 (safe beside a live host). Prints which
/// of the two HID paths answered. Drivers: `punktfunk-host.exe driver install --gamepad`.
#[cfg(target_os = "windows")]
pub fn channel_proof_probe(_args: &[String]) -> Result<()> {
    crate::inject::mouse_windows::channel_proof_probe()
}

/// Virtual DualSense via UMDF (`SwDeviceCreate` + shared-memory channel). No session.
///
/// `Get-PnpDevice` VID_054C. Exit closes the devnode. Same entry for DS4 / Xbox / Edge /
/// Deck / Triton via flags. `--idle-after` / `--resume-after` are Moonlight's change-only
/// cadence.
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
    // `--index N` → `pf_pad_N` (default 0). Use a spare if the host already holds 0.
    let idx: u8 = args
        .iter()
        .skip_while(|a| *a != "--index")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let ds4 = args.iter().any(|a| a == "--ds4");
    let xbox = args.iter().any(|a| a == "--xbox");
    let xboxhid = args.iter().any(|a| a == "--xboxhid");
    // `--xboxones` / `--xboxelite`: same report, different VID/PID. `02FD` has no
    // stage-2 `HID\…&IG_00` in `xinputhid.inf` — watch promotion vs `0B13`.
    let xboxones = args.iter().any(|a| a == "--xboxones");
    let xboxelite = args.iter().any(|a| a == "--xboxelite");
    // `--edge`: paddles on pressed beats (report byte 10 = 0x80|0x40). `--deck`:
    // MI_02 Steam Deck. `--triton`: raw_len=0, so a stick sweep is the 0x42 fallback.
    let edge = args.iter().any(|a| a == "--edge");
    let deck = args.iter().any(|a| a == "--deck");
    // `--idle-after N`: stop State frames, keep pumping — Moonlight sends only on CHANGE.
    // Native never hits this: `input_task.rs` re-sends every pad every 100 ms.
    let idle_after: u64 = args
        .iter()
        .skip_while(|a| *a != "--idle-after")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // `--resume-after M`: write State again. A listed pad after silence is not proof —
    // `win-input-matrix --watch` timestamps must advance.
    let resume_after: u64 = args
        .iter()
        .skip_while(|a| *a != "--resume-after")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let triton = args.iter().any(|a| a == "--triton");
    let extra_buttons: u32 = if edge || deck || triton {
        punktfunk_core::input::gamepad::BTN_PADDLE1 | punktfunk_core::input::gamepad::BTN_PADDLE2
    } else {
        0
    };
    macro_rules! drive {
        ($mgr:expr, $label:expr) => {{
            let mut mgr = $mgr;
            mgr.handle(&GamepadEvent::Arrival {
                index: idx,
                kind: 2,
                capabilities: 0,
                audio_caps: 0,
            });
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
            let started = Instant::now();
            let mut announced_silence = false;
            let mut announced_resume = false;
            let (mut i, mut last) = (0i32, Instant::now());
            while Instant::now() < deadline {
                mgr.pump(
                    |pad, lo, hi, lt, rt| println!(
                        "  rumble from game: pad={pad} low={lo} high={hi} lt={lt} rt={rt}"
                    ),
                    |o| println!("  hid output from game: {o:?}"),
                );
                let el = started.elapsed();
                let resumed =
                    resume_after != 0 && el >= Duration::from_secs(resume_after.max(idle_after));
                let silent =
                    idle_after != 0 && el >= Duration::from_secs(idle_after) && !resumed;
                if silent && !announced_silence {
                    announced_silence = true;
                    println!(
                        "  --- going SILENT (no more state frames, still pumping) at {}s ---",
                        idle_after
                    );
                }
                if resumed && !announced_resume {
                    announced_resume = true;
                    println!(
                        "  --- RESUMING state frames at {}s (after {}s of silence) ---",
                        resume_after,
                        resume_after.saturating_sub(idle_after)
                    );
                }
                if !silent && last.elapsed() >= Duration::from_millis(400) {
                    last = Instant::now();
                    i += 1;
                    let buttons = if i % 2 == 0 {
                        punktfunk_core::input::gamepad::BTN_A | extra_buttons
                    } else {
                        0
                    };
                    // Distinct phase per axis + opposing trigger ramps: a dead axis and an
                    // undriven one look the same; same-phase would hide crosstalk.
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
        // XUSB: handle + pump_rumble, no HID-output plane — cannot use `drive!`.
        let mut mgr = crate::inject::gamepad::GamepadManager::new();
        mgr.handle(&GamepadEvent::Arrival {
            index: idx,
            kind: 1,
            capabilities: 0,
            audio_caps: 0,
        });
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
            // XUSB rumble has no trigger motors (`pump_rumble`); printed to match the HID line.
            mgr.pump_rumble(|pad, lo, hi, lt, rt| {
                println!("  rumble from game: pad={pad} low={lo} high={hi} lt={lt} rt={rt}")
            });
            t += 1;
            let lx = (((t % 200) - 100) * 327).clamp(-32768, 32767) as i16; // ±32700, just under i16 full scale
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
        // Shipping SwDeviceCreate identity, not devgen: a devgen HID child is
        // `HID\VID_045E&UP:0001_U:0005` with no PID, so Windows never promotes it.
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
    } else if triton {
        drive!(
            crate::inject::triton_windows::TritonWindowsManager::new(),
            "Steam Controller 2"
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

/// Audio-substrate toolbox (`windows-audio-endpoints-and-vbcable.md`).
///
/// `audio-probe ssm|sink|sss-primary|mint|plan|cleanup [--keep]`. `ssm` mints a second
/// Steam Streaming Microphone and proves render→capture; `sink` parks default on a minted
/// Speakers and loopback-measures; `sss-primary` re-measures the primary Speakers;
/// `mint` runs the provider; `plan` prints one wiring pass and its readiness verdict.
#[cfg(target_os = "windows")]
pub fn audio_probe(args: &[String]) -> Result<()> {
    crate::audio::audio_probe::run(args)
}

/// Pad-audio endpoint: `ensure|remove|status|tone|capture|show|hide [--index N]`.
///
/// `ensure` is the startup path (reuse-or-create, bind Steam Streaming Speakers, stamp
/// DualSense 4ch/48k). `remove` is the pnputil escape hatch — endpoints persist.
/// Host parks them hidden with no client pad; `show` before `tone`/`capture`.
/// Stamping needs SYSTEM (MMDevices ACL): service account or PsExec.
#[cfg(target_os = "windows")]
pub fn pad_endpoint(args: &[String]) -> Result<()> {
    use crate::audio::pad_endpoint as pe;
    let idx: u8 = args
        .iter()
        .skip_while(|a| *a != "--index")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // `--endpoint <id>`: any render device. Same binary on a known-good id separates
    // "this process cannot activate" from "our endpoint is broken".
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
        // Direct WASAPI render — a game launch cannot say which link in the chain broke.
        Some("tone") => {
            let secs: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
            let hz: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(60.0);
            let endpoint_id = match endpoint_override {
                Some(id) => id,
                None => {
                    // `find` is a system lookup. `endpoint_for` is the service's in-process
                    // cache; this CLI has none.
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
            // `--pair front`: speaker pair, not voice coils. No game renders the speaker kind.
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
        // Receiving half of `tone`. Run both to prove render → engine → loopback with no game.
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
        // DEVICE_STATE_DISABLED. Host parks pads hidden: idle libScePad titles stall on a
        // visible endpoint. `tone`/`capture` need it shown first.
        Some(verb @ ("show" | "hide")) => {
            let endpoint_id = match endpoint_override {
                Some(id) => id,
                None => match pe::find(idx)? {
                    Some(ep) if !ep.endpoint_id.is_empty() => ep.endpoint_id,
                    _ => {
                        println!("pad-endpoint {verb}: pad {idx} has no endpoint — run `ensure`");
                        return Ok(());
                    }
                },
            };
            pe::set_visibility(&endpoint_id, idx, verb == "show");
            println!("pad-endpoint {verb}: {endpoint_id}");
            Ok(())
        }
        _ => anyhow::bail!(
            "usage: punktfunk-host pad-endpoint \
             <ensure|remove|status|tone|capture|show|hide> [--index N]"
        ),
    }
}

/// Mirror a named physical monitor and pull frames (`design/per-monitor-portal-capture.md`).
///
/// Same display-backend open as a session (`PUNKTFUNK_CAPTURE_MONITOR` routes to mirror).
/// Proves the compositor accepted a record request for that head and it produces pixels
/// at its own size. `--monitor <CONNECTOR>` (else the pin); `--seconds N`.
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
    // `--monitor` cannot set `PUNKTFUNK_CAPTURE_MONITOR`: config is snapshotted at
    // startup. Explicit connector → `open_mirror`; unset → pin / production `open`.
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

    let mut vd = match &explicit {
        Some(connector) => crate::vdisplay::open_mirror(compositor, connector)?,
        None => crate::vdisplay::open(compositor)?,
    };
    // Mirror ignores mode (the panel runs at its owner's). Pass the head's own if the pin drops.
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

    // Default: session GPU/dmabuf. `--cpu` forces mmap — different PipeWire buffer types.
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
            // Timeout is not fatal: screencast is damage-driven; a static desktop produces
            // nothing for seconds. Wait the full `--seconds`, don't stop on the first gap.
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

/// Absolute input at a named monitor (`design/per-monitor-portal-capture.md`).
///
/// Two same-size heads: matching a libei region by streamed mode can pick the wrong
/// screen. This uses the compositor's EIS regions and prints the mapped output.
/// `--monitor <CONNECTOR>` (else the pin); `--none` is unanchored A/B. Walks
/// `--width`×`--height` corners. Answer: `libei: absolute input maps into this output`.
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
        // WlrVirtual (Sway) would green-pass with no region log. Need a compositor that speaks EI.
        anyhow::bail!(
            "input backend is {backend:?}, not libei — the absolute-region ladder only exists on \
             the libei backend; set PUNKTFUNK_INPUT_BACKEND=libei"
        );
    }
    let mut inj = crate::inject::open(backend)?;
    // 4 s: libei portal/EIS + device resume; events before that drop, and resume publishes regions.
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
