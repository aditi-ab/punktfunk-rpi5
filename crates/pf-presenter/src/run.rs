//! The session lifecycle loop: one SDL context on the caller's main thread driving the
//! window, the Vulkan presenter, input capture, the pumped gamepad service, and the
//! shared session pump's event/frame channels.
//!
//! Two modes over one loop: **single** (`run_session` — one `--connect` stream, exit on
//! end; the shell↔session contract) and **browse** (`run_browse` — the console library
//! idles between streams; overlay actions launch sessions, session end returns to the
//! library; the app quits only on B/window-close).
//!
//! Stdout is the machine interface (the shell↔session contract): one `{"ready":true}`
//! line after the first presented frame, `stats: …` lines once per window while enabled
//! (Ctrl+Alt+Shift+S toggles). Logs go to stderr (the binary configures tracing so).

use crate::input::Capture;
use crate::overlay::{FrameCtx, Overlay, OverlayAction, OverlayFrame, SessionPhase};
use crate::vk::{FrameInput, Presenter};
use anyhow::{Context as _, Result};
use pf_client_core::gamepad::GamepadService;
use pf_client_core::session::{self, SessionEvent, SessionHandle, SessionParams, Stats};
use pf_client_core::video::VulkanDecodeDevice;
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
    /// The window's top-left in desktop coordinates; `None` = centered on the primary
    /// display. The shells pass their own window's position so the stream opens on the
    /// SAME monitor (and the shell⇄session visibility handoff reads as one window
    /// changing content, not a window jumping displays). Fullscreen follows the display
    /// this lands on.
    pub window_pos: Option<(i32, i32)>,
    /// Print `stats:` lines (Ctrl+Alt+Shift+S toggles live).
    pub print_stats: bool,
    /// Emit the `{"ready":true}` stdout line after the first presented frame.
    pub json_status: bool,
    /// Called once on `Connected` with the host's fingerprint (trust persistence is the
    /// binary's business — this loop stays store-agnostic).
    pub on_connected: Option<Box<dyn FnMut([u8; 32])>>,
    /// The console-UI overlay (§6.1) — `None` is the Skia-free power-user build (stats
    /// stay stdout-only). An overlay whose `init` fails degrades to `None` with a
    /// warning rather than killing the session. Browse mode requires one.
    pub overlay: Option<Box<dyn Overlay>>,
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

/// What the session binary decided about an overlay action (browse mode).
pub enum ActionOutcome {
    /// Consumed binary-side (a Retry respawned the fetch, …).
    Handled,
    /// Start this session (a Launch action; `force_software` from the callback args is
    /// wired into these params). Boxed: SessionParams is large next to the unit variants.
    Start(Box<SessionParams>),
    /// Quit the launcher.
    Quit,
}

/// One `--connect` stream session; returns when it ends (the shell↔session contract).
pub fn run_session<F>(opts: SessionOpts, build_params: F) -> Result<Outcome>
where
    F: FnOnce(&GamepadService, Mode, Arc<AtomicBool>, Option<VulkanDecodeDevice>) -> SessionParams,
{
    let mut build = Some(build_params);
    run_inner(
        opts,
        ModeCtl::Single(Box::new(move |gp, native, fs, vk| {
            (build.take().expect("single build runs once"))(gp, native, fs, vk)
        })),
    )
    .map(|o| o.expect("single mode always yields an outcome"))
}

/// Browse mode: the console library idles between streams. `on_action` receives every
/// overlay action (Launch/Retry/Quit) plus what a launch needs to build its params —
/// the gamepad service (`auto_pref`), the native display mode, and a fresh
/// per-session `force_software` flag.
pub fn run_browse<F>(opts: SessionOpts, on_action: F) -> Result<()>
where
    F: FnMut(
        OverlayAction,
        &GamepadService,
        Mode,
        Arc<AtomicBool>,
        Option<VulkanDecodeDevice>,
    ) -> ActionOutcome,
{
    anyhow::ensure!(
        opts.overlay.is_some(),
        "--browse needs the console UI (a build with the `ui` feature)"
    );
    run_inner(opts, ModeCtl::Browse(Box::new(on_action))).map(|_| ())
}

/// Params builder for the one single-mode session (called exactly once, post-setup).
type BuildParams<'a> = Box<
    dyn FnMut(&GamepadService, Mode, Arc<AtomicBool>, Option<VulkanDecodeDevice>) -> SessionParams
        + 'a,
>;
/// The browse-mode action callback (Launch → params, Retry/Quit → outcome).
type OnAction<'a> = Box<
    dyn FnMut(
            OverlayAction,
            &GamepadService,
            Mode,
            Arc<AtomicBool>,
            Option<VulkanDecodeDevice>,
        ) -> ActionOutcome
        + 'a,
>;

/// The two run modes, type-erased so one loop serves both.
enum ModeCtl<'a> {
    Single(BuildParams<'a>),
    Browse(OnAction<'a>),
}

/// The custom SDL event a decoded frame's arrival pushes (see [`StreamState::new`]):
/// pure wake-up — the loop drains the frame channel regardless of why it woke.
struct FrameWake;

/// Everything one stream session accumulates — created at session start, dropped at
/// session end (browse mode cycles through several per process lifetime).
struct StreamState {
    handle: SessionHandle,
    /// Decoded frames, re-queued by the wake forwarder (newest-wins, like the pump's
    /// own queue). The loop drains THIS, never `handle.frames` — the forwarder is that
    /// channel's one consumer.
    frames: async_channel::Receiver<DecodedFrame>,
    connector: Option<Arc<NativeClient>>,
    capture: Option<Capture>,
    force_software: Arc<AtomicBool>,
    /// The user canceled this connect from the console — never engage the stream
    /// (skip capture/attach on a late `Connected`) and route its end back silently.
    canceled: bool,
    ready_announced: bool,
    mode_line: String,
    clock_offset_ns: i64,
    hdr: bool,
    // Presenter-side 1 s window (design/stats-unification.md): end-to-end
    // capture→displayed (host-clock corrected) p50+p95, display = decoded→displayed p50.
    win_e2e_us: Vec<u64>,
    win_disp_us: Vec<u64>,
    win_start: Instant,
    presented: PresentedWindow,
    // Hardware-path health: a failure streak (or a device with no import support at
    // all) demotes the decoder to software via the shared flag — once per session.
    dmabuf_demoted: bool,
    hw_fails: u32,
    /// The OSD's text (multi-line; rebuilt each Stats window).
    osd_text: String,
}

impl StreamState {
    /// `wake`: pushes a [`FrameWake`] SDL event as each decoded frame lands, via a tiny
    /// forwarder thread that owns the pump's frame channel. This is what lets the run
    /// loop BLOCK in `wait_event_timeout` (instead of a 1 ms poll — measured as a full
    /// core burned at any frame rate) yet still present a frame the instant it arrives:
    /// input events and frames both wake the same wait. The forwarder exits when the
    /// pump drops its sender (session end/shutdown).
    fn new(
        params: SessionParams,
        force_software: Arc<AtomicBool>,
        wake: sdl3::event::EventSender,
    ) -> StreamState {
        let handle = session::start(params);
        let (wake_tx, wake_rx) = async_channel::bounded(2);
        let pump_rx = handle.frames.clone();
        let _ = std::thread::Builder::new()
            .name("pf-frame-wake".into())
            .spawn(move || {
                while let Ok(f) = pump_rx.recv_blocking() {
                    let _ = wake_tx.force_send(f); // newest wins, like the pump's queue
                    let _ = wake.push_custom_event(FrameWake);
                }
            });
        StreamState {
            handle,
            frames: wake_rx,
            connector: None,
            capture: None,
            force_software,
            canceled: false,
            ready_announced: false,
            mode_line: String::new(),
            clock_offset_ns: 0,
            hdr: false,
            win_e2e_us: Vec::with_capacity(256),
            win_disp_us: Vec::with_capacity(256),
            win_start: Instant::now(),
            presented: PresentedWindow::default(),
            dmabuf_demoted: false,
            hw_fails: 0,
            osd_text: String::new(),
        }
    }

    /// Stop the pump and JOIN its thread — required before any device-wide idle or
    /// teardown (the pump submits decode work to the shared device). Quick: the pump
    /// notices `stop` within its 20 ms receive timeout, and on a normal end it's
    /// already returning.
    fn shutdown(mut self) {
        self.handle.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.handle.thread.take() {
            let _ = t.join();
        }
    }

    /// Deliberate user exit (chord / window close): release capture, close with
    /// QUIT_CLOSE_CODE so the host tears down instead of lingering, stop the pump.
    /// The pump then emits `Ended(None)` — the loop's normal end path picks it up.
    fn request_quit(&mut self) {
        if let Some(cap) = &mut self.capture {
            cap.release(true);
        }
        if let Some(c) = &self.connector {
            c.disconnect_quit();
        }
        self.handle.stop.store(true, Ordering::SeqCst);
    }
}

/// Whether a present error is `VK_ERROR_DEVICE_LOST` anywhere in its chain. A lost
/// device is unrecoverable by spec — every object on it (decoder frames, swapchain,
/// the Skia context) is dead, and the demote-to-software path would rebuild the
/// decoder against that same dead device (observed live 2026-07-09: FFmpeg wedges
/// inside the rebuild, the decode thread never returns, and the client zombies with
/// the pump flushing a never-draining backlog every 2 s). The only correct response
/// is to fail the session loudly and let the shell relaunch.
fn device_lost(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|c| c.downcast_ref::<ash::vk::Result>() == Some(&ash::vk::Result::ERROR_DEVICE_LOST))
}

fn run_inner(mut opts: SessionOpts, mut mode: ModeCtl) -> Result<Option<Outcome>> {
    // Before any window exists: unpackaged runs adopt the shell's AppUserModelID so the
    // shell⇄session windows group as one taskbar app (win32.rs; MSIX identity wins).
    #[cfg(windows)]
    crate::win32::set_app_user_model_id();
    sdl3::hint::set("SDL_JOYSTICK_THREAD", "1");
    let sdl = sdl3::init().context("SDL init")?;
    let video = sdl.video().context("SDL video")?;
    let events = sdl.event().context("SDL events")?;
    events
        .register_custom_event::<FrameWake>()
        .map_err(|e| anyhow::anyhow!("register FrameWake event: {e}"))?;
    let mut window = {
        let mut b = video.window(&opts.window_title, 1280, 720);
        match opts.window_pos {
            Some((x, y)) => b.position(x, y),
            None => b.position_centered(),
        };
        b.resizable().vulkan();
        if opts.fullscreen {
            b.fullscreen();
        }
        b.build().context("SDL window")?
    };
    // The exe-embedded icon onto the title bar/taskbar/Alt-Tab (SDL's class icon is the
    // generic default); a no-op for exes that embed none.
    #[cfg(windows)]
    crate::win32::stamp_window_icon(&window);
    let instance_exts = window
        .vulkan_instance_extensions()
        .map_err(|e| anyhow::anyhow!("vulkan instance extensions: {e}"))?;
    let mut presenter = Presenter::new(&window, &instance_exts).context("vulkan presenter")?;
    // A valid black frame immediately — the window is honest while the connect runs.
    presenter.present(&window, FrameInput::Redraw, None)?;
    // Browse mode is "ready" the moment the library window presents — there may never be
    // a stream. (Single mode announces on the first VIDEO frame instead, further down, so
    // a shell only yields to a window that actually shows the stream.)
    if opts.json_status && matches!(mode, ModeCtl::Browse(_)) {
        println!("{{\"ready\":true}}");
    }

    let mut overlay = opts.overlay.take();
    if let Some(o) = overlay.as_mut() {
        if let Err(e) = o.init(&presenter.shared_device()) {
            if matches!(mode, ModeCtl::Browse(_)) {
                return Err(e).context("console UI init (required for --browse)");
            }
            tracing::warn!(error = %format!("{e:#}"),
                "console-UI overlay init failed — continuing without it");
            overlay = None;
        }
    }

    let gamepad_subsystem = sdl.gamepad().context("SDL gamepad")?;
    let (gamepad, mut pump) = GamepadService::pumped(gamepad_subsystem);
    let escape_rx = gamepad.escape_events();
    let disconnect_rx = gamepad.disconnect_events();
    let menu_rx = gamepad.menu_events();
    if matches!(mode, ModeCtl::Browse(_)) {
        // Menu mode for the launcher's lifetime (an attached session supersedes
        // translation automatically — the GTK launcher never turned it off either).
        gamepad.set_menu_mode(true);
    }

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

    let mut stream: Option<StreamState> = match &mut mode {
        ModeCtl::Single(build) => {
            let force_software = Arc::new(AtomicBool::new(false));
            let params = build(
                &gamepad,
                native,
                force_software.clone(),
                presenter.vulkan_decode(),
            );
            Some(StreamState::new(
                params,
                force_software,
                events.event_sender(),
            ))
        }
        ModeCtl::Browse(_) => None,
    };

    let mut event_pump = sdl
        .event_pump()
        .map_err(|e| anyhow::anyhow!("SDL event pump: {e}"))?;
    let mouse = sdl.mouse();

    let mut fullscreen = opts.fullscreen;
    let mut print_stats = opts.print_stats;
    let mut overlay_frame: Option<OverlayFrame> = None;
    // SDL text input tracks the overlay's editing state (started = IME/`TextInput`
    // events on desktop, and the door Steam's on-screen keyboard types through under
    // gamescope). Toggled edge-wise — start/stop are not free on Wayland.
    let mut text_input_on = false;

    let outcome = 'main: loop {
        // --- SDL events (input, window, gamepads) ---------------------------------------
        // Block in SDL's own wait: input/window events AND decoded frames (the wake
        // forwarder's FrameWake) all land in this one queue, so the loop wakes exactly
        // when there is work — a short-timeout poll here burned a full core (measured;
        // the timeout only bounds stop-flag/pump-tick latency now). In browse-idle the
        // per-iteration FIFO present vsync-throttles the loop anyway.
        let timeout = Duration::from_millis(15);
        let first = event_pump.wait_event_timeout(timeout);
        let mut queued: Vec<Event> = Vec::new();
        if let Some(e) = first {
            queued.push(e);
        }
        while let Some(e) = event_pump.poll_event() {
            queued.push(e);
        }
        for event in queued {
            // The console UI sees input first: a consumed event (the library's keyboard
            // navigation, a menu) never reaches capture/forwarding.
            if let Some(o) = overlay.as_mut() {
                if o.handle_event(&event) {
                    continue;
                }
            }
            match event {
                Event::Quit { .. } => {
                    // Window close / SIGINT: deliberate exit, host teardown now.
                    if let Some(st) = &mut stream {
                        st.request_quit();
                    }
                    break 'main Some(Outcome::Ended(None));
                }
                Event::Window { win_event, .. } => match win_event {
                    WindowEvent::FocusLost => {
                        if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                            if cap.release(false) {
                                apply_capture(&mut window, &mouse, false);
                                tracing::info!("focus lost — input released");
                            }
                        }
                    }
                    WindowEvent::FocusGained => {
                        // An auto-release (Alt-Tab) undoes itself; a chord release
                        // stays released until the user opts back in.
                        if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                            if cap.should_reengage() {
                                cap.engage();
                                apply_capture(&mut window, &mouse, true);
                                tracing::info!("focus gained — input recaptured");
                            }
                        }
                    }
                    WindowEvent::PixelSizeChanged(..) | WindowEvent::Resized(..) => {
                        presenter.recreate_swapchain(&window)?;
                        presenter.present(&window, FrameInput::Redraw, overlay_frame.as_ref())?;
                    }
                    WindowEvent::Exposed => {
                        presenter.present(&window, FrameInput::Redraw, overlay_frame.as_ref())?;
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
                        if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                            if cap.captured() {
                                cap.release(true);
                                apply_capture(&mut window, &mouse, false);
                            } else {
                                cap.engage();
                                apply_capture(&mut window, &mouse, true);
                            }
                            tracing::info!(captured = cap.captured(), "chord: release/engage");
                        }
                        continue;
                    }
                    if chord && sc == Scancode::D {
                        if let Some(st) = &mut stream {
                            tracing::info!("chord: disconnect");
                            st.request_quit();
                            apply_capture(&mut window, &mouse, false);
                            // The pump emits Ended(None); the end path routes per mode.
                        }
                        continue;
                    }
                    if chord && sc == Scancode::S {
                        print_stats = !print_stats;
                        continue;
                    }
                    // F11 or Alt+Enter (some keyboards' Fn layer sends a media key for
                    // plain F11 — the Moonlight-standard alias always exists).
                    let alt_enter =
                        sc == Scancode::Return && keymod.intersects(Mod::LALTMOD | Mod::RALTMOD);
                    if sc == Scancode::F11 || alt_enter {
                        fullscreen = !fullscreen;
                        tracing::debug!(fullscreen, "fullscreen toggle");
                        if let Err(e) = window.set_fullscreen(fullscreen) {
                            tracing::warn!(error = %e, "fullscreen toggle");
                        }
                        continue;
                    }
                    if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                        cap.on_key_down(sc);
                    }
                }
                Event::KeyUp {
                    scancode: Some(sc), ..
                } => {
                    if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                        cap.on_key_up(sc);
                    }
                }
                Event::MouseMotion { xrel, yrel, .. } => {
                    if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                        cap.on_motion(xrel, yrel);
                    }
                }
                Event::MouseButtonDown { mouse_btn, .. } => {
                    if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                        if !cap.captured() {
                            // The engaging click is suppressed toward the host.
                            cap.engage();
                            apply_capture(&mut window, &mouse, true);
                        } else {
                            cap.on_button_down(mouse_btn);
                        }
                    }
                }
                Event::MouseButtonUp { mouse_btn, .. } => {
                    if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                        cap.on_button_up(mouse_btn);
                    }
                }
                Event::MouseWheel { x, y, .. } => {
                    if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                        cap.on_wheel(x, y);
                    }
                }
                // The wake forwarder's FrameWake (and any other user event): pure
                // wake-up — the frame drain below runs this iteration either way.
                Event::User { .. } => {}
                // Everything else (gamepad add/remove/button/axis/touchpad/sensor…) is
                // the pumped gamepad worker's — it ignores what it doesn't know.
                other => pump.handle_event(other),
            }
        }
        pump.tick();
        // One coalesced MouseMove per iteration — pure motion must reach the host
        // without waiting for a click/key to flush it.
        if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
            cap.flush_motion();
        }

        // Text input follows the overlay's editing state (edge-triggered).
        let want_text = overlay.as_ref().is_some_and(|o| o.text_input_active());
        if want_text != text_input_on {
            text_input_on = want_text;
            let ti = video.text_input();
            if want_text {
                ti.start(&window);
            } else {
                ti.stop(&window);
            }
        }

        // Controller escape chord: release capture (+ leave fullscreen on desktop — under
        // a `--fullscreen` gamescope launch there is nothing to release into). Only
        // emitted while a session is attached.
        while escape_rx.try_recv().is_ok() {
            if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                if cap.release(true) {
                    apply_capture(&mut window, &mouse, false);
                }
            }
            if fullscreen && !opts.fullscreen {
                fullscreen = false;
                let _ = window.set_fullscreen(false);
            }
        }
        // Escape chord held past the threshold: the controller's Ctrl+Alt+Shift+D.
        if disconnect_rx.try_recv().is_ok() {
            if let Some(st) = &mut stream {
                tracing::info!("controller chord: disconnect");
                st.request_quit();
                apply_capture(&mut window, &mouse, false);
            }
        }

        // --- Browse: menu navigation + overlay actions (console visible only) ------------
        if let ModeCtl::Browse(on_action) = &mut mode {
            // Menu events flow while no stream is engaged — including a connect in
            // flight (connector still None), so B can cancel the dial. Once attached,
            // the gamepad worker forwards raw input instead of translating.
            if stream.as_ref().is_none_or(|s| s.connector.is_none()) {
                while let Ok(ev) = menu_rx.try_recv() {
                    if let Some(o) = overlay.as_mut() {
                        if let Some(pulse) = o.handle_menu(ev) {
                            gamepad.menu_rumble(pulse);
                        }
                    }
                }
            }
            if let Some(action) = overlay.as_mut().and_then(|o| o.take_action()) {
                match action {
                    OverlayAction::CancelConnect => {
                        if let Some(st) = &mut stream {
                            if st.connector.is_none() && !st.canceled {
                                tracing::info!("connect canceled from the console");
                                st.canceled = true;
                                st.handle.stop.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                    action => {
                        let force_software = Arc::new(AtomicBool::new(false));
                        match on_action(
                            action,
                            &gamepad,
                            native,
                            force_software.clone(),
                            presenter.vulkan_decode(),
                        ) {
                            ActionOutcome::Handled => {}
                            ActionOutcome::Start(params) => {
                                stream = Some(StreamState::new(
                                    *params,
                                    force_software,
                                    events.event_sender(),
                                ));
                                if let Some(o) = overlay.as_mut() {
                                    o.session_phase(SessionPhase::Connecting);
                                }
                            }
                            ActionOutcome::Quit => break Some(Outcome::Ended(None)),
                        }
                    }
                }
            }
        }

        // --- Session events --------------------------------------------------------------
        // `stream` may become None mid-drain (browse-mode session end) — re-borrow each
        // event, act, and stop draining on the terminal ones.
        while let Some(st) = stream.as_mut() {
            let Ok(ev) = st.handle.events.try_recv() else {
                break;
            };
            match ev {
                SessionEvent::Connected {
                    connector: c,
                    mode: m,
                    fingerprint,
                } => {
                    if st.canceled {
                        // The dial won the race against the cancel: quit-close the host
                        // side now; the stop flag (already set) ends the pump and the
                        // Ended path routes back to the console without ever engaging.
                        c.disconnect_quit();
                        continue;
                    }
                    st.mode_line = format!("{}×{}@{}", m.width, m.height, m.refresh_hz);
                    tracing::info!(mode = %st.mode_line, "connected");
                    window
                        .set_title(&format!("{} · {}", opts.window_title, st.mode_line))
                        .ok();
                    gamepad.attach(c.clone());
                    st.clock_offset_ns = c.clock_offset_ns;
                    let mut cap = Capture::new(c.clone());
                    cap.engage(); // capture engages when the stream starts (ui_stream parity)
                    apply_capture(&mut window, &mouse, true);
                    st.capture = Some(cap);
                    st.connector = Some(c);
                    if let Some(f) = opts.on_connected.as_mut() {
                        f(fingerprint);
                    }
                    if let Some(o) = overlay.as_mut() {
                        o.session_phase(SessionPhase::Streaming);
                    }
                }
                SessionEvent::Stats(s) => {
                    st.osd_text = stats_text(
                        &st.mode_line,
                        &s,
                        &st.presented,
                        st.hdr,
                        presenter.hdr_active(),
                    );
                    if print_stats {
                        println!("stats: {}", st.osd_text.replace('\n', " | "));
                    }
                }
                SessionEvent::Failed {
                    msg,
                    trust_rejected,
                } => match &mode {
                    ModeCtl::Single(_) => {
                        break 'main Some(Outcome::ConnectFailed {
                            msg,
                            trust_rejected,
                        })
                    }
                    ModeCtl::Browse(_) => {
                        tracing::warn!(%msg, "connect failed — back to the console");
                        let canceled = st.canceled;
                        if let Some(st) = stream.take() {
                            st.shutdown();
                        }
                        apply_capture(&mut window, &mouse, false);
                        if let Some(o) = overlay.as_mut() {
                            // A user-canceled dial ends silently — no error scene.
                            if canceled {
                                o.session_phase(SessionPhase::Ended(None));
                            } else {
                                o.session_phase(SessionPhase::Failed(&msg));
                            }
                        }
                        break;
                    }
                },
                SessionEvent::Ended(reason) => {
                    gamepad.detach();
                    if let Some(cap) = &mut st.capture {
                        cap.release(true);
                    }
                    apply_capture(&mut window, &mouse, false);
                    match &mode {
                        ModeCtl::Single(_) => break 'main Some(Outcome::Ended(reason)),
                        ModeCtl::Browse(_) => {
                            window.set_title(&opts.window_title).ok();
                            let canceled = st.canceled;
                            if let Some(st) = stream.take() {
                                st.shutdown();
                            }
                            if let Some(o) = overlay.as_mut() {
                                // A canceled connect's end carries no reason strip.
                                o.session_phase(SessionPhase::Ended(if canceled {
                                    None
                                } else {
                                    reason.as_deref()
                                }));
                            }
                            break;
                        }
                    }
                }
            }
        }

        // --- Console UI: damage-driven overlay re-render for this iteration --------------
        if let Some(o) = overlay.as_mut() {
            let (pw, ph) = window.size_in_pixels();
            let (stats, hint) = match &stream {
                Some(st) if st.connector.is_some() => {
                    let hint = match &st.capture {
                        Some(cap) if !cap.captured() => Some(if gamepad.active().is_some() {
                            HINT_WITH_PAD
                        } else {
                            HINT_KEYBOARD
                        }),
                        _ => None,
                    };
                    (
                        (print_stats && !st.osd_text.is_empty()).then_some(st.osd_text.as_str()),
                        hint,
                    )
                }
                _ => (None, None),
            };
            let pad = gamepad.active();
            let pads = gamepad.pads();
            let ctx = FrameCtx {
                width: pw,
                height: ph,
                stats,
                hint,
                pad: pad.as_ref().map(|p| p.name.as_str()),
                pad_pref: pad.as_ref().map(|p| p.pref),
                pads: &pads,
            };
            match o.frame(&ctx) {
                Ok(f) => overlay_frame = f,
                Err(e) => {
                    if matches!(mode, ModeCtl::Browse(_)) {
                        return Err(e).context("console UI frame (required for --browse)");
                    }
                    tracing::warn!(error = %format!("{e:#}"),
                        "overlay frame failed — disabling the console UI");
                    overlay = None;
                    overlay_frame = None;
                }
            }
        }

        // --- Frames: drain to the newest, upload + present -------------------------------
        let mut presented_video = false;
        if let Some(st) = &mut stream {
            // Mastering metadata (0xCE) → the presentation engine, ahead of the frame
            // that needs it. Low-rate (session start + mastering changes / keyframes).
            if let Some(c) = &st.connector {
                while let Ok(m) = c.next_hdr_meta(Duration::ZERO) {
                    presenter.set_hdr_metadata(m);
                }
            }
            let mut newest: Option<DecodedFrame> = None;
            while let Ok(f) = st.frames.try_recv() {
                newest = Some(f);
            }
            if let Some(f) = newest {
                let DecodedFrame {
                    pts_ns,
                    decoded_ns,
                    image,
                } = f;
                let did_present = match image {
                    DecodedImage::Cpu(c) => {
                        st.hdr = c.color.is_pq();
                        presenter.present(&window, FrameInput::Cpu(&c), overlay_frame.as_ref())?
                    }
                    #[cfg(target_os = "linux")]
                    DecodedImage::Dmabuf(d)
                        if presenter.supports_dmabuf() && !st.dmabuf_demoted =>
                    {
                        st.hdr = d.color.is_pq();
                        match presenter.present(
                            &window,
                            FrameInput::Dmabuf(d),
                            overlay_frame.as_ref(),
                        ) {
                            Ok(p) => {
                                st.hw_fails = 0;
                                p
                            }
                            // Import/CSC failure is survivable (the stream continues on
                            // the next frame) — but a streak means this box can't do the
                            // hw path: demote the decoder to software, same contract as
                            // the GTK presenter's GL-converter failures. A lost DEVICE
                            // is not survivable and must not demote — see [`device_lost`].
                            Err(e) => {
                                if device_lost(&e) {
                                    return Err(e)
                                        .context("GPU device lost — the session cannot continue");
                                }
                                st.hw_fails += 1;
                                tracing::warn!(error = %format!("{e:#}"), fails = st.hw_fails,
                                    "hardware present failed");
                                if st.hw_fails >= 3 && !st.dmabuf_demoted {
                                    st.dmabuf_demoted = true;
                                    tracing::warn!("demoting the decoder to software");
                                    st.force_software.store(true, Ordering::Relaxed);
                                }
                                false
                            }
                        }
                    }
                    #[cfg(target_os = "linux")]
                    DecodedImage::Dmabuf(_) => {
                        // No import extensions on this device (or already demoted) — the
                        // pump rebuilds the decoder as software; frames flow again soon.
                        if !st.dmabuf_demoted {
                            st.dmabuf_demoted = true;
                            tracing::warn!(
                                "no dmabuf import support on this device — demoting the \
                                 decoder to software"
                            );
                            st.force_software.store(true, Ordering::Relaxed);
                        }
                        false
                    }
                    // D3D11VA: shared-texture import, same gate + failure-streak
                    // demotion contract as the dmabuf path.
                    #[cfg(windows)]
                    DecodedImage::D3d11(d) if presenter.supports_d3d11() && !st.dmabuf_demoted => {
                        st.hdr = d.color.is_pq();
                        match presenter.present(
                            &window,
                            FrameInput::D3d11(d),
                            overlay_frame.as_ref(),
                        ) {
                            Ok(p) => {
                                st.hw_fails = 0;
                                p
                            }
                            Err(e) => {
                                // Lost device ⇒ unrecoverable, never demote ([`device_lost`]).
                                if device_lost(&e) {
                                    return Err(e)
                                        .context("GPU device lost — the session cannot continue");
                                }
                                st.hw_fails += 1;
                                tracing::warn!(error = %format!("{e:#}"), fails = st.hw_fails,
                                    "hardware present failed");
                                if st.hw_fails >= 3 && !st.dmabuf_demoted {
                                    st.dmabuf_demoted = true;
                                    tracing::warn!("demoting the decoder to software");
                                    st.force_software.store(true, Ordering::Relaxed);
                                }
                                false
                            }
                        }
                    }
                    #[cfg(windows)]
                    DecodedImage::D3d11(_) => {
                        // No import extensions on this device (or already demoted) — the
                        // pump rebuilds the decoder as software; frames flow again soon.
                        if !st.dmabuf_demoted {
                            st.dmabuf_demoted = true;
                            tracing::warn!(
                                "no win32 external-memory import on this device — demoting \
                                 the decoder to software"
                            );
                            st.force_software.store(true, Ordering::Relaxed);
                        }
                        false
                    }
                    // Vulkan-Video: decoded on the presenter's own device — present is
                    // views + CSC, no import step to gate on. Same failure-streak
                    // demotion contract as the dmabuf path.
                    DecodedImage::VkFrame(v) if !st.dmabuf_demoted => {
                        st.hdr = v.color.is_pq();
                        match presenter.present(
                            &window,
                            FrameInput::VkFrame(v),
                            overlay_frame.as_ref(),
                        ) {
                            Ok(p) => {
                                st.hw_fails = 0;
                                p
                            }
                            Err(e) => {
                                // Lost device ⇒ unrecoverable, never demote ([`device_lost`]).
                                if device_lost(&e) {
                                    return Err(e)
                                        .context("GPU device lost — the session cannot continue");
                                }
                                st.hw_fails += 1;
                                tracing::warn!(error = %format!("{e:#}"), fails = st.hw_fails,
                                    "vulkan-video present failed");
                                if st.hw_fails >= 3 {
                                    st.dmabuf_demoted = true;
                                    tracing::warn!("demoting the decoder to software");
                                    st.force_software.store(true, Ordering::Relaxed);
                                }
                                false
                            }
                        }
                    }
                    DecodedImage::VkFrame(_) => false, // demoted — drain until rebuild
                };
                if did_present {
                    presented_video = true;
                    let displayed_ns = session::now_ns();
                    if opts.json_status && !st.ready_announced {
                        st.ready_announced = true;
                        println!("{{\"ready\":true}}");
                    }
                    // The `displayed` stamp (same clamp rules as the pump's windows).
                    let e2e = (displayed_ns as i128 + st.clock_offset_ns as i128 - pts_ns as i128)
                        .max(0) as u64;
                    if e2e > 0 && e2e < 10_000_000_000 {
                        st.win_e2e_us.push(e2e / 1000);
                    }
                    st.win_disp_us
                        .push(displayed_ns.saturating_sub(decoded_ns) / 1000);
                }
            }

            // Fold the presenter window into the shared stats line once per second.
            if st.win_start.elapsed() >= Duration::from_secs(1) {
                let (e2e_p50, e2e_p95) = session::window_percentiles(&mut st.win_e2e_us);
                let (disp_p50, _) = session::window_percentiles(&mut st.win_disp_us);
                st.presented = PresentedWindow {
                    e2e_p50_ms: e2e_p50 as f32 / 1000.0,
                    e2e_p95_ms: e2e_p95 as f32 / 1000.0,
                    display_ms: disp_p50 as f32 / 1000.0,
                };
                st.win_e2e_us.clear();
                st.win_disp_us.clear();
                st.win_start = Instant::now();
            }
        }

        // Browse with no video driving presents (library / connecting): composite the
        // overlay every iteration — FIFO vsync-throttles this to the display rate.
        if matches!(mode, ModeCtl::Browse(_))
            && !presented_video
            && stream.as_ref().is_none_or(|s| s.connector.is_none())
        {
            presenter.present(&window, FrameInput::Redraw, overlay_frame.as_ref())?;
        }
    };

    // Join the pump BEFORE the device-wide idle: its decode submissions on the shared
    // device would race vkDeviceWaitIdle otherwise.
    if let Some(st) = stream.take() {
        st.shutdown();
    }
    // Overlay resources live on the presenter's device: quiesce the queue first, drop
    // the overlay (its Drop destroys the Skia surfaces), THEN the presenter tears down.
    presenter.wait_idle();
    drop(overlay);
    Ok(outcome)
}

/// Apply the capture state to the window: pointer lock (relative mouse + hidden cursor)
/// and — on Windows — a keyboard grab, so system chords (Alt+Tab, the Windows key) reach
/// the host while captured instead of the local shell. SDL implements the grab there
/// with a low-level keyboard hook, the same mechanism the WinUI shell's in-process
/// client used its own WH_KEYBOARD_LL hooks for. Not engaged on Linux: the compositor
/// shortcut-inhibit story stays the shells' concern (Settings.inhibit_shortcuts).
fn apply_capture(window: &mut sdl3::video::Window, mouse: &sdl3::mouse::MouseUtil, on: bool) {
    mouse.set_relative_mouse_mode(window, on);
    mouse.show_cursor(!on);
    #[cfg(windows)]
    window.set_keyboard_grab(on);
}

/// The presenter's share of the unified stats window — folded into each printed line.
#[derive(Default)]
struct PresentedWindow {
    e2e_p50_ms: f32,
    e2e_p95_ms: f32,
    display_ms: f32,
}

/// The capture hints (`ui_stream` parity — the words the user reads while released).
const HINT_KEYBOARD: &str = "Click the stream to capture input · Ctrl+Alt+Shift+Q releases · \
     Ctrl+Alt+Shift+D disconnects · Ctrl+Alt+Shift+S stats";
const HINT_WITH_PAD: &str = "Click the stream to capture input · Ctrl+Alt+Shift+Q releases · \
     Ctrl+Alt+Shift+D disconnects · hold L1 + R1 + Start + Select to leave";

/// The unified stats window (design/stats-unification.md) as OSD text — multi-line for
/// the console-UI panel; the stdout `stats:` line joins it with `|`.
///
/// The HDR tag is honest about the display path: `HDR` only when the swapchain actually
/// runs HDR10 (`hdr_display`); a PQ stream tone-mapped onto an SDR surface (no HDR10
/// format offered, HDR off in the compositor) shows `HDR→SDR` instead.
fn stats_text(
    mode_line: &str,
    s: &Stats,
    p: &PresentedWindow,
    hdr_stream: bool,
    hdr_display: bool,
) -> String {
    let mut text = format!(
        "{mode_line} · {:.0} fps · {:.1} Mb/s · {}{}",
        s.fps,
        s.mbps,
        if s.decoder.is_empty() { "-" } else { s.decoder },
        match (hdr_stream, hdr_display) {
            (true, true) => " · HDR",
            (true, false) => " · HDR→SDR",
            _ => "",
        },
    );
    text.push_str(&format!(
        "\ne2e {:.1}/{:.1} ms (p50/p95)",
        p.e2e_p50_ms, p.e2e_p95_ms
    ));
    if s.split {
        text.push_str(&format!(" · host {:.1} · net {:.1}", s.host_ms, s.net_ms));
    } else {
        text.push_str(&format!(" · host+net {:.1}", s.host_net_ms));
    }
    text.push_str(&format!(
        " · decode {:.1} · display {:.1} ms",
        s.decode_ms, p.display_ms
    ));
    if s.lost > 0 {
        text.push_str(&format!("\nlost {} ({:.1}%)", s.lost, s.lost_pct));
    }
    text
}
