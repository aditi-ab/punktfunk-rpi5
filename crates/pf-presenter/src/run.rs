//! The session lifecycle loop: one SDL context on the caller's main thread driving the
//! window, the Vulkan presenter, input capture, the pumped gamepad service, and the
//! shared session pump's event/frame channels.
//!
//! Stdout is the machine interface (the shell↔session contract): one `{"ready":true}`
//! line after the first presented frame, `stats: …` lines once per window while enabled
//! (Ctrl+Alt+Shift+S toggles). Logs go to stderr (the binary configures tracing so).

use crate::input::Capture;
use crate::vk::Presenter;
use anyhow::{Context as _, Result};
use pf_client_core::gamepad::GamepadService;
use pf_client_core::session::{self, SessionEvent, SessionParams, Stats};
use pf_client_core::video::{DecodedFrame, DecodedImage};
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::Mode;
use sdl3::event::{Event, WindowEvent};
use sdl3::keyboard::Mod;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct SessionOpts {
    pub window_title: String,
    /// Start fullscreen (gamescope / `--fullscreen`).
    pub fullscreen: bool,
    /// Print `stats:` lines (Ctrl+Alt+Shift+S toggles live).
    pub print_stats: bool,
    /// Emit the `{"ready":true}` stdout line after the first presented frame.
    pub json_status: bool,
    /// Called once on `Connected` with the host's fingerprint (trust persistence is the
    /// binary's business — this loop stays store-agnostic).
    pub on_connected: Option<Box<dyn FnMut([u8; 32])>>,
}

pub enum Outcome {
    /// The session ran and ended: `None` = deliberate exit (user quit), `Some` = the
    /// reason the pump reported (host ended, transport error…).
    Ended(Option<String>),
    ConnectFailed {
        msg: String,
        trust_rejected: bool,
    },
}

/// Everything SDL on the main thread, per the plan (§5.1): window + events + gamepads in
/// ONE event loop — the shell-era worker-thread arrangement can't coexist with an SDL
/// video window. `build_params` receives the gamepad service (for `auto_pref`), the
/// window's native display mode (the `0 = native` resolution fallback), and the shared
/// `force_software` flag, and returns the pump parameters.
pub fn run_session<F>(mut opts: SessionOpts, build_params: F) -> Result<Outcome>
where
    F: FnOnce(&GamepadService, Mode, Arc<AtomicBool>) -> SessionParams,
{
    sdl3::hint::set("SDL_JOYSTICK_THREAD", "1");
    let sdl = sdl3::init().context("SDL init")?;
    let video = sdl.video().context("SDL video")?;
    let mut window = {
        let mut b = video.window(&opts.window_title, 1280, 720);
        b.position_centered().resizable().vulkan();
        if opts.fullscreen {
            b.fullscreen();
        }
        b.build().context("SDL window")?
    };
    let instance_exts = window
        .vulkan_instance_extensions()
        .map_err(|e| anyhow::anyhow!("vulkan instance extensions: {e}"))?;
    let mut presenter = Presenter::new(&window, &instance_exts).context("vulkan presenter")?;
    // A valid black frame immediately — the window is honest while the connect runs.
    presenter.present(&window, None)?;

    let gamepad_subsystem = sdl.gamepad().context("SDL gamepad")?;
    let (gamepad, mut pump) = GamepadService::pumped(gamepad_subsystem);
    let escape_rx = gamepad.escape_events();
    let disconnect_rx = gamepad.disconnect_events();

    // The native display mode — the `0 = native` fallback for the requested stream mode
    // (the GTK client reads the monitor under its window; same idea).
    let native = window
        .get_display()
        .and_then(|d| d.get_mode())
        .map(|m| Mode {
            width: m.w.max(0) as u32,
            height: m.h.max(0) as u32,
            refresh_hz: m.refresh_rate.round().max(0.0) as u32,
        })
        .unwrap_or(Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        });

    let force_software = Arc::new(AtomicBool::new(false));
    let params = build_params(&gamepad, native, force_software.clone());
    let handle = session::start(params);

    let mut event_pump = sdl
        .event_pump()
        .map_err(|e| anyhow::anyhow!("SDL event pump: {e}"))?;
    let mouse = sdl.mouse();

    // --- Loop state --------------------------------------------------------------------
    let mut connector: Option<Arc<NativeClient>> = None;
    let mut capture: Option<Capture> = None;
    let mut fullscreen = opts.fullscreen;
    let mut ready_announced = false;
    let mut print_stats = opts.print_stats;
    let mut mode_line = String::new();
    let mut clock_offset_ns: i64 = 0;
    let mut hdr = false;
    // Presenter-side 1 s window (design/stats-unification.md): end-to-end
    // capture→displayed (host-clock corrected) p50+p95, display = decoded→displayed p50.
    let mut win_e2e_us: Vec<u64> = Vec::with_capacity(256);
    let mut win_disp_us: Vec<u64> = Vec::with_capacity(256);
    let mut win_start = Instant::now();
    let mut presented = PresentedWindow::default();
    // Retained newest frame for expose/resize re-blits (the presenter keeps the GPU
    // image; this keeps the pending upload if one arrives while minimized).
    let mut dmabuf_demoted = false;

    let outcome = 'main: loop {
        // --- SDL events (input, window, gamepads) ---------------------------------------
        // Block briefly in SDL's own wait so idle costs nothing; while streaming, frames
        // arrive on the channel below and 1 ms bounds the added present latency.
        let timeout = Duration::from_millis(if connector.is_some() { 1 } else { 10 });
        let first = event_pump.wait_event_timeout(timeout);
        let mut queued: Vec<Event> = Vec::new();
        if let Some(e) = first {
            queued.push(e);
        }
        while let Some(e) = event_pump.poll_event() {
            queued.push(e);
        }
        for event in queued {
            match event {
                Event::Quit { .. } => {
                    // Window close / SIGINT: deliberate user exit — QUIT_CLOSE_CODE so
                    // the host tears down instead of lingering for a reconnect.
                    if let Some(c) = &connector {
                        c.disconnect_quit();
                    }
                    handle.stop.store(true, Ordering::SeqCst);
                    break 'main Outcome::Ended(None);
                }
                Event::Window { win_event, .. } => match win_event {
                    WindowEvent::FocusLost => {
                        if let Some(cap) = &mut capture {
                            if cap.release() {
                                mouse.show_cursor(true);
                                tracing::info!("focus lost — input released");
                            }
                        }
                    }
                    WindowEvent::PixelSizeChanged(..) | WindowEvent::Resized(..) => {
                        presenter.recreate_swapchain(&window)?;
                        presenter.present(&window, None)?;
                    }
                    WindowEvent::Exposed => {
                        presenter.present(&window, None)?;
                    }
                    _ => {}
                },
                Event::KeyDown {
                    scancode: Some(sc),
                    keymod,
                    repeat: false,
                    ..
                } => {
                    let chord = keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD)
                        && keymod.intersects(Mod::LALTMOD | Mod::RALTMOD)
                        && keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD);
                    use sdl3::keyboard::Scancode;
                    if chord && sc == Scancode::Q {
                        if let Some(cap) = &mut capture {
                            if cap.captured() {
                                cap.release();
                                mouse.show_cursor(true);
                            } else {
                                cap.engage();
                                mouse.show_cursor(false);
                            }
                            tracing::info!(captured = cap.captured(), "chord: release/engage");
                        }
                        continue;
                    }
                    if chord && sc == Scancode::D {
                        tracing::info!("chord: disconnect");
                        if let Some(cap) = &mut capture {
                            cap.release();
                        }
                        if let Some(c) = &connector {
                            c.disconnect_quit();
                        }
                        handle.stop.store(true, Ordering::SeqCst);
                        break 'main Outcome::Ended(None);
                    }
                    if chord && sc == Scancode::S {
                        print_stats = !print_stats;
                        continue;
                    }
                    if sc == Scancode::F11 {
                        fullscreen = !fullscreen;
                        if let Err(e) = window.set_fullscreen(fullscreen) {
                            tracing::warn!(error = %e, "fullscreen toggle");
                        }
                        continue;
                    }
                    if let Some(cap) = &mut capture {
                        cap.on_key_down(sc, window.size());
                    }
                }
                Event::KeyUp {
                    scancode: Some(sc), ..
                } => {
                    if let Some(cap) = &mut capture {
                        cap.on_key_up(sc);
                    }
                }
                Event::MouseMotion { x, y, .. } => {
                    if let Some(cap) = &mut capture {
                        cap.on_motion(x, y);
                    }
                }
                Event::MouseButtonDown {
                    mouse_btn, x, y, ..
                } => {
                    if let Some(cap) = &mut capture {
                        if !cap.captured() {
                            // The engaging click is suppressed toward the host.
                            cap.engage();
                            mouse.show_cursor(false);
                        } else {
                            cap.on_button_down(mouse_btn, x, y, window.size());
                        }
                    }
                }
                Event::MouseButtonUp { mouse_btn, .. } => {
                    if let Some(cap) = &mut capture {
                        cap.on_button_up(mouse_btn, window.size());
                    }
                }
                Event::MouseWheel { x, y, .. } => {
                    if let Some(cap) = &mut capture {
                        cap.on_wheel(x, y, window.size());
                    }
                }
                // Everything else (gamepad add/remove/button/axis/touchpad/sensor…) is
                // the pumped gamepad worker's — it ignores what it doesn't know.
                other => pump.handle_event(other),
            }
        }
        pump.tick();

        // Controller escape chord: release capture (+ leave fullscreen on desktop — under
        // a `--fullscreen` gamescope launch there is nothing to release into).
        while escape_rx.try_recv().is_ok() {
            if let Some(cap) = &mut capture {
                if cap.release() {
                    mouse.show_cursor(true);
                }
            }
            if fullscreen && !opts.fullscreen {
                fullscreen = false;
                let _ = window.set_fullscreen(false);
            }
        }
        // Escape chord held past the threshold: the controller's Ctrl+Alt+Shift+D.
        if disconnect_rx.try_recv().is_ok() {
            tracing::info!("controller chord: disconnect");
            if let Some(cap) = &mut capture {
                cap.release();
            }
            if let Some(c) = &connector {
                c.disconnect_quit();
            }
            handle.stop.store(true, Ordering::SeqCst);
            break Outcome::Ended(None);
        }

        // --- Session events --------------------------------------------------------------
        while let Ok(ev) = handle.events.try_recv() {
            match ev {
                SessionEvent::Connected {
                    connector: c,
                    mode,
                    fingerprint,
                } => {
                    mode_line = format!("{}×{}@{}", mode.width, mode.height, mode.refresh_hz);
                    tracing::info!(mode = %mode_line, "connected");
                    window.set_title(&format!("{} · {}", opts.window_title, mode_line)).ok();
                    gamepad.attach(c.clone());
                    clock_offset_ns = c.clock_offset_ns;
                    let mut cap = Capture::new(c.clone());
                    cap.engage(); // capture engages when the stream starts (ui_stream parity)
                    mouse.show_cursor(false);
                    capture = Some(cap);
                    connector = Some(c);
                    if let Some(f) = opts.on_connected.as_mut() {
                        f(fingerprint);
                    }
                }
                SessionEvent::Stats(s) => {
                    if print_stats {
                        print_stats_line(&mode_line, &s, &presented, hdr);
                    }
                }
                SessionEvent::Failed { msg, trust_rejected } => {
                    break 'main Outcome::ConnectFailed { msg, trust_rejected };
                }
                SessionEvent::Ended(reason) => {
                    gamepad.detach();
                    if let Some(cap) = &mut capture {
                        cap.release();
                        mouse.show_cursor(true);
                    }
                    break 'main Outcome::Ended(reason);
                }
            }
        }

        // --- Frames: drain to the newest, upload + present -------------------------------
        let mut newest: Option<DecodedFrame> = None;
        while let Ok(f) = handle.frames.try_recv() {
            newest = Some(f);
        }
        if let Some(f) = newest {
            match f.image {
                DecodedImage::Cpu(c) => {
                    hdr = c.color.is_pq();
                    presenter.present(&window, Some(&c))?;
                    let displayed_ns = session::now_ns();
                    if opts.json_status && !ready_announced {
                        ready_announced = true;
                        println!("{{\"ready\":true}}");
                    }
                    // The `displayed` stamp (same clamp rules as the pump's windows).
                    let e2e = (displayed_ns as i128 + clock_offset_ns as i128 - f.pts_ns as i128)
                        .max(0) as u64;
                    if e2e > 0 && e2e < 10_000_000_000 {
                        win_e2e_us.push(e2e / 1000);
                    }
                    win_disp_us.push(displayed_ns.saturating_sub(f.decoded_ns) / 1000);
                }
                DecodedImage::Dmabuf(_) => {
                    // Phase 1 has no hardware present path — demote the decoder to
                    // software through the same contract the GTK presenter uses. Once.
                    if !dmabuf_demoted {
                        dmabuf_demoted = true;
                        tracing::warn!(
                            "hardware (dmabuf) frame reached the phase-1 presenter — \
                             demoting the decoder to software"
                        );
                        force_software.store(true, Ordering::Relaxed);
                    }
                }
            }
        }

        // Fold the presenter window into the shared stats line once per second.
        if win_start.elapsed() >= Duration::from_secs(1) {
            let (e2e_p50, e2e_p95) = session::window_percentiles(&mut win_e2e_us);
            let (disp_p50, _) = session::window_percentiles(&mut win_disp_us);
            presented = PresentedWindow {
                e2e_p50_ms: e2e_p50 as f32 / 1000.0,
                e2e_p95_ms: e2e_p95 as f32 / 1000.0,
                display_ms: disp_p50 as f32 / 1000.0,
            };
            win_e2e_us.clear();
            win_disp_us.clear();
            win_start = Instant::now();
        }
    };

    handle.stop.store(true, Ordering::SeqCst);
    Ok(outcome)
}

/// The presenter's share of the unified stats window — folded into each printed line.
#[derive(Default)]
struct PresentedWindow {
    e2e_p50_ms: f32,
    e2e_p95_ms: f32,
    display_ms: f32,
}

/// One `stats:` line per 1 s window — the OSD's numbers (design/stats-unification.md) in
/// terminal form: headline end-to-end capture→displayed, then the per-stage p50s.
fn print_stats_line(mode_line: &str, s: &Stats, p: &PresentedWindow, hdr: bool) {
    let mut line = format!(
        "stats: {mode_line} · {:.0} fps · {:.1} Mb/s · {}{} · e2e {:.1}/{:.1} ms (p50/p95)",
        s.fps,
        s.mbps,
        if s.decoder.is_empty() { "-" } else { s.decoder },
        if hdr { " · HDR" } else { "" },
        p.e2e_p50_ms,
        p.e2e_p95_ms,
    );
    if s.split {
        line.push_str(&format!(
            " · host {:.1} · net {:.1}",
            s.host_ms, s.net_ms
        ));
    } else {
        line.push_str(&format!(" · host+net {:.1}", s.host_net_ms));
    }
    line.push_str(&format!(
        " · decode {:.1} · display {:.1} ms",
        s.decode_ms, p.display_ms
    ));
    if s.lost > 0 {
        line.push_str(&format!(" · lost {} ({:.1}%)", s.lost, s.lost_pct));
    }
    println!("{line}");
}
