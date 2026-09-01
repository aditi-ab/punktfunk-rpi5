//! The console host proper: one render thread that owns the EGL context, the Skia
//! `DirectContext` and the [`Console`], paced by `eglSwapBuffers` while a surface is up
//! and parked while there is none. Everything else — Kotlin's input, surface lifecycle,
//! session edges — arrives through a command queue and is applied on that thread; what the
//! console raises (actions, haptic pulses, editing state, settings to persist) leaves
//! through an event queue a Kotlin poll thread blocks on.
//!
//! The model side needs none of this: `ConsoleShared`/`LibraryShared` are lock-guarded and
//! written straight from JNI, `ConsoleBus` is drained straight from JNI. Only the shell
//! itself is single-threaded, and this thread is that thread.

use super::egl::{EglContext, EglSurface, GlesVersion};
use super::gpu::Gpu;
use anyhow::{bail, Result};
use ndk::native_window::NativeWindow;
use pf_client_core::console::{OverlayAction, PointerInput, SessionPhase};
use pf_client_core::menu_nav::{MenuEvent, MenuNav, MenuPulse, MenuSample, PadInfo};
use pf_console_ui::{
    Console, ConsoleEntry, ConsoleHandles, ConsoleOptions, InputSource, Insets, Key, SnapshotStore,
    Viewport,
};
use punktfunk_core::config::GamepadPref;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A session edge as Kotlin reports it — `SessionPhase` borrows its strings, so the queue
/// carries an owned twin.
pub(super) enum Phase {
    Connecting,
    Streaming,
    Failed(String),
    Ended(Option<String>),
    Reconnecting(String),
}

/// What Kotlin asks the render thread to do.
pub(super) enum Cmd {
    Menu(MenuEvent),
    /// The raw pad, whenever it changes; the thread feeds `MenuNav` with the LAST sample every
    /// frame (repeats need a clock) and once on arrival (a press must not wait for a frame).
    PadSample(MenuSample),
    Pointer(PointerInput),
    Key {
        key: Key,
        shift: bool,
        repeat: bool,
    },
    Text(String),
    Phase(Phase),
    Navigate(ConsoleEntry),
    SurfaceCreated(NativeWindow),
    SurfaceChanged,
    /// Acknowledged through `Shared::surface_gen` once the EGL surface is really gone —
    /// Kotlin's `surfaceDestroyed` must not return before that.
    SurfaceDestroyed,
    Viewport {
        insets: Insets,
        scale: Option<f64>,
    },
    Pads {
        label: Option<String>,
        pref: Option<GamepadPref>,
        pads: Vec<PadInfo>,
    },
    Quit,
}

/// What the render thread raises for Kotlin.
pub(super) enum HostEvent {
    Action(OverlayAction),
    Pulse(MenuPulse),
    Editing(bool),
    /// The shell saved settings: here is the whole snapshot to persist.
    Settings(Box<pf_client_core::trust::Settings>),
    /// The GLES generation the context came up with — Kotlin logs it, nothing more.
    Gles(GlesVersion),
    /// The render thread died (EGL/Skia init failed). Kotlin falls back to its own console.
    Dead(String),
}

impl HostEvent {
    /// The JSON Kotlin parses. Hand-rolled for the small variants; the two model payloads
    /// ride serde.
    pub(super) fn to_json(&self) -> String {
        match self {
            HostEvent::Action(a) => format!(
                "{{\"action\":{}}}",
                serde_json::to_string(a).unwrap_or_else(|_| "null".into())
            ),
            HostEvent::Pulse(p) => format!(
                "{{\"pulse\":\"{}\"}}",
                match p {
                    MenuPulse::Move => "move",
                    MenuPulse::Confirm => "confirm",
                    MenuPulse::Boundary => "boundary",
                }
            ),
            HostEvent::Editing(e) => format!("{{\"editing\":{e}}}"),
            HostEvent::Settings(s) => format!(
                "{{\"settings\":{}}}",
                serde_json::to_string(s).unwrap_or_else(|_| "null".into())
            ),
            HostEvent::Gles(v) => format!(
                "{{\"gles\":{}}}",
                match v {
                    GlesVersion::Es2 => 2,
                    GlesVersion::Es3 => 3,
                }
            ),
            HostEvent::Dead(msg) => format!(
                "{{\"dead\":{}}}",
                serde_json::to_string(msg).unwrap_or_else(|_| "\"\"".into())
            ),
        }
    }
}

pub(super) struct Shared {
    inbox: Mutex<VecDeque<Cmd>>,
    inbox_cv: Condvar,
    events: Mutex<VecDeque<HostEvent>>,
    events_cv: Condvar,
    /// Bumped by the render thread each time it has torn a surface down; `SurfaceDestroyed`
    /// waits for the bump.
    surface_gen: Mutex<u64>,
    surface_cv: Condvar,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            inbox: Mutex::new(VecDeque::new()),
            inbox_cv: Condvar::new(),
            events: Mutex::new(VecDeque::new()),
            events_cv: Condvar::new(),
            surface_gen: Mutex::new(0),
            surface_cv: Condvar::new(),
        }
    }

    pub(super) fn send(&self, cmd: Cmd) {
        self.inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(cmd);
        self.inbox_cv.notify_one();
    }

    fn emit(&self, ev: HostEvent) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(ev);
        self.events_cv.notify_one();
    }

    /// Kotlin's poll: the next event, waiting up to `timeout` for one.
    pub(super) fn next_event(&self, timeout: Duration) -> Option<HostEvent> {
        let mut q = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if q.is_empty() {
            let (guard, _) = self
                .events_cv
                .wait_timeout(q, timeout)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            q = guard;
        }
        q.pop_front()
    }

    /// Ask for the surface to go and wait (bounded) until it has.
    pub(super) fn destroy_surface_blocking(&self) {
        let before = *self
            .surface_gen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.send(Cmd::SurfaceDestroyed);
        let g = self
            .surface_gen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Bounded: a render thread that died mid-frame must not hang the UI thread forever —
        // by then the EGL surface is gone with it anyway.
        let _ = self
            .surface_cv
            .wait_timeout_while(g, Duration::from_secs(2), |g| *g == before)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }

    fn ack_surface_gone(&self) {
        *self
            .surface_gen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        self.surface_cv.notify_all();
    }
}

/// The host as the JNI layer holds it.
pub(super) struct ConsoleHost {
    pub(super) shared: Arc<Shared>,
    pub(super) handles: ConsoleHandles,
    pub(super) store: Arc<SnapshotStore>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ConsoleHost {
    /// Spawn the render owner, which builds Skia state on-thread and parks for a surface.
    /// Thread creation errors return to JNI; later console/build failures emit `Dead` events.
    pub(super) fn start(
        opts: ConsoleOptions,
        entry: ConsoleEntry,
        store: Arc<SnapshotStore>,
    ) -> std::io::Result<ConsoleHost> {
        let shared = Arc::new(Shared::new());
        let handles = ConsoleHandles::new();
        let thread_shared = shared.clone();
        let thread_store = store.clone();
        let thread_handles = handles.clone();
        let thread = std::thread::Builder::new()
            .name("pf-console".into())
            .spawn(move || {
                boost_thread_priority();
                let run = || -> Result<()> {
                    let console = Console::new(opts, entry, &thread_handles)?;
                    render_loop(console, thread_shared.clone(), thread_store)
                };
                if let Err(e) = run() {
                    log::error!("console: render thread ended: {e:#}");
                    thread_shared.emit(HostEvent::Dead(format!("{e:#}")));
                }
            })?;
        Ok(ConsoleHost {
            shared,
            handles,
            store,
            thread: Some(thread),
        })
    }

    /// Signal the render loop and join it once. The final table-held `Arc` calls this from `Drop`.
    fn stop(&mut self) {
        self.shared.send(Cmd::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ConsoleHost {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Best-effort: lift the console's render thread off the default nice band, the same way
/// `decode::setup::boost_thread_priority` lifts the decode thread. This thread IS the console's
/// frame loop — every menu press waits on it — and at default priority a TV box's scheduler is
/// free to park it on a little core behind whatever else the system is doing, which reads as a
/// UI that lags the remote. `-8` rather than the decode path's `-10`: a stream's frames are the
/// harder deadline, and the two should not compete when the console is up during a session.
///
/// Non-fatal if the platform refuses (the exact floor a foreground app may set is policy).
fn boost_thread_priority() {
    // SAFETY: `gettid`/`setpriority` on the calling thread are always-safe syscalls; PRIO_PROCESS
    // with a TID targets that one task on Linux — the idiom `Process.setThreadPriority` uses.
    unsafe {
        let tid = libc::gettid();
        if libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, -8) != 0 {
            log::debug!(
                "console: setpriority(-8) failed (non-fatal): {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// How often the render loop reports what a frame is costing it. Nothing in a bug report from a
/// TV said whether the console was drawing at 4K or at 60 Hz, so "it feels sluggish" could not be
/// triaged from a log bundle at all — this is that missing line. One line a minute is cheap
/// enough to leave on for everyone, and the answer is only useful from the box that is slow.
const FRAME_REPORT: Duration = Duration::from_secs(60);

/// No input for this long = the console is being looked at, not used — halve the redraw
/// rate (`IDLE_FRAME_STEP` slept between swaps). 60 s keeps every interaction and its
/// afterglow at full smoothness and only calms a genuinely parked screen.
const IDLE_AFTER: Duration = Duration::from_secs(60);
/// One extra ~vsync period per frame while idle: 60 Hz → ~30, 120 Hz → ~40.
const IDLE_FRAME_STEP: Duration = Duration::from_millis(16);

/// The render thread. Owns EGL + Skia + the console; runs until `Cmd::Quit`.
fn render_loop(mut console: Console, shared: Arc<Shared>, store: Arc<SnapshotStore>) -> Result<()> {
    let egl = EglContext::new()?;
    shared.emit(HostEvent::Gles(egl.version));
    let mut gpu: Option<Gpu> = None;
    let mut window: Option<NativeWindow> = None;
    let mut surface: Option<EglSurface> = None;
    let mut skia: Option<(skia_safe::Surface, u32, u32)> = None;
    let mut nav = MenuNav::new();
    let mut sample = MenuSample::default();
    let mut insets = Insets::default();
    let mut scale: Option<f64> = None;
    let mut pad_label: Option<String> = None;
    let mut pad_pref: Option<GamepadPref> = None;
    let mut pads: Vec<PadInfo> = Vec::new();
    let mut was_editing = console.editing();
    let mut saved_gen = store.saved_gen();
    let mut menu_out: Vec<MenuEvent> = Vec::new();
    // When the last input arrived — the idle throttle's clock (see the draw site below).
    let mut last_input = Instant::now();
    // Consecutive GL setup failures (window surface / Skia wrap). One is a transient (a window
    // torn down mid-create); a run of them is a context that is not coming back — most likely
    // reclaimed by Android while the app was backgrounded. Only exiting reports that: each
    // failure alone is logged, the loop retries, and the screen stays a gray never-painted
    // SurfaceView forever. Dying raises `Dead`, and Kotlin answers with the touch UI.
    let mut gl_failures = 0u32;
    const GL_FAILURE_LIMIT: u32 = 3;
    // What a frame is costing, reported once a `FRAME_REPORT` window (see there).
    let (mut frames, mut frame_time, mut frame_peak) = (0u32, Duration::ZERO, Duration::ZERO);
    let mut report_at = Instant::now();

    loop {
        // Take everything queued. With no surface up, block until something arrives.
        let cmds: Vec<Cmd> = {
            let mut q = shared
                .inbox
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if surface.is_none() && q.is_empty() {
                let (guard, _) = shared
                    .inbox_cv
                    .wait_timeout(q, Duration::from_millis(500))
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                q = guard;
            }
            q.drain(..).collect()
        };
        let mut poll_now = false;
        for cmd in cmds {
            match cmd {
                Cmd::Quit => {
                    // Release in order: the Skia surface, then the current binding, then (on
                    // return) the EGL surface + window + context.
                    drop(skia.take());
                    if surface.is_some() {
                        egl.release_current();
                    }
                    return Ok(());
                }
                Cmd::Menu(ev) => {
                    last_input = Instant::now();
                    // Discrete events are the remote/keyboard path (Kotlin routes pad
                    // buttons through PadSample) — with one wrinkle: a pad's SELECT also
                    // arrives here (SkiaConsoleShell's ▲-on-Home shortcut), briefly
                    // reading as keys. The next real pad press corrects the legend.
                    if let Some(p) = console.menu(ev, InputSource::Keys) {
                        shared.emit(HostEvent::Pulse(p));
                    }
                }
                Cmd::PadSample(s) => {
                    last_input = Instant::now();
                    sample = s;
                    poll_now = true;
                }
                Cmd::Pointer(p) => {
                    last_input = Instant::now();
                    console.pointer(p);
                }
                Cmd::Key { key, shift, repeat } => {
                    last_input = Instant::now();
                    console.key(key, shift, repeat);
                }
                Cmd::Text(t) => {
                    last_input = Instant::now();
                    console.text(&t);
                }
                Cmd::Phase(ph) => {
                    match &ph {
                        Phase::Connecting => console.session_phase(SessionPhase::Connecting),
                        Phase::Streaming => console.session_phase(SessionPhase::Streaming),
                        Phase::Failed(m) => console.session_phase(SessionPhase::Failed(m)),
                        Phase::Ended(r) => {
                            console.session_phase(SessionPhase::Ended(r.as_deref()));
                        }
                        Phase::Reconnecting(m) => {
                            console.session_phase(SessionPhase::Reconnecting(m));
                        }
                    }
                    // Coming back from a stream: whatever is held on the pad now (the chord
                    // that ended it) must be released before it can act here.
                    if matches!(ph, Phase::Ended(_) | Phase::Failed(_)) {
                        nav.reset();
                    }
                }
                Cmd::Navigate(entry) => console.navigate(entry),
                Cmd::SurfaceCreated(w) => {
                    // A surface arriving while one is up: replace it (Kotlin re-created the
                    // view without a destroy in between — treat as destroy + create).
                    if surface.is_some() {
                        skia = None;
                        egl.release_current();
                        surface = None;
                        window = None;
                    }
                    match egl.window_surface(w.ptr().as_ptr().cast()) {
                        Ok(s) => {
                            if gpu.is_none() {
                                gpu = Some(Gpu::new(&egl, console.gpu_cache_bytes())?);
                            }
                            surface = Some(s);
                            window = Some(w);
                            gl_failures = 0;
                            // A fresh surface is a fresh entry: snapshot the pad so a button
                            // still held from before does not fire into the first frame.
                            nav.reset();
                        }
                        Err(e) => {
                            log::error!("console: window surface: {e:#}");
                            gl_failures += 1;
                        }
                    }
                }
                Cmd::SurfaceChanged => {
                    if let Some(s) = surface.as_mut() {
                        s.refresh_size();
                    }
                }
                Cmd::SurfaceDestroyed => {
                    skia = None;
                    if surface.is_some() {
                        egl.release_current();
                    }
                    surface = None;
                    window = None;
                    shared.ack_surface_gone();
                }
                Cmd::Viewport {
                    insets: i,
                    scale: s,
                } => {
                    insets = i;
                    scale = s;
                }
                Cmd::Pads {
                    label,
                    pref,
                    pads: p,
                } => {
                    pad_label = label;
                    pad_pref = pref;
                    pads = p;
                }
            }
        }
        // `window` is only held so the ANativeWindow outlives the EGL surface over it.
        let _ = &window;

        // The pad, through the shared synthesizer: once per frame for repeats, plus once
        // right now if a sample just arrived.
        if poll_now || surface.is_some() {
            menu_out.clear();
            nav.poll(&sample, Instant::now(), &mut menu_out);
            for ev in menu_out.drain(..) {
                if let Some(p) = console.menu(ev, InputSource::Pad) {
                    shared.emit(HostEvent::Pulse(p));
                }
            }
        }

        // Draw, if there is somewhere to draw.
        // ponytail: half-rate after 60 s without input — one extra frame period between
        // swaps, so an idle carousel stops redrawing a phone's panel at its full rate
        // (the aurora still breathes, at half tempo). Any input restores full rate on
        // its own frame; damage-driven rendering if a TV box ever needs more.
        if last_input.elapsed() >= IDLE_AFTER {
            std::thread::sleep(IDLE_FRAME_STEP);
        }
        if let (Some(s), Some(g)) = (surface.as_mut(), gpu.as_mut()) {
            let (w, h) = (s.width, s.height);
            let need_wrap = match &skia {
                Some((_, sw, sh)) => *sw != w || *sh != h,
                None => true,
            };
            if need_wrap {
                skia = None;
                match g.wrap_window(&egl, w, h) {
                    Ok(surf) => {
                        // The console's real render resolution — the one number a bug report
                        // from a TV never carried. A 4K panel is 4× the fragment work of 1080p
                        // for every pass the shell draws.
                        log::info!("console: drawing at {w}×{h}");
                        skia = Some((surf, w, h));
                        gl_failures = 0;
                        // Start the frame window here, not at loop entry: the console parks
                        // with no surface while a stream is up, and a window that had been
                        // open across that would report its first frame as "1 frame in 20 min".
                        (frames, frame_time, frame_peak, report_at) =
                            (0, Duration::ZERO, Duration::ZERO, Instant::now());
                    }
                    Err(e) => {
                        log::error!("console: {e:#}");
                        gl_failures += 1;
                    }
                }
            }
            if let Some((surf, _, _)) = skia.as_mut() {
                let viewport = Viewport {
                    width: w,
                    height: h,
                    insets,
                    scale,
                };
                // Around the DRAW only, not the swap: `eglSwapBuffers` blocks on vsync, so
                // wall-clock per iteration is always ~the panel period and says nothing. What
                // matters is how much of that period the shell spends building the frame —
                // once that passes the period, the console is missing vsyncs.
                let drew = Instant::now();
                console.frame(
                    surf.canvas(),
                    &viewport,
                    pad_label.as_deref(),
                    pad_pref,
                    &pads,
                );
                g.context.flush_and_submit();
                let cost = drew.elapsed();
                frame_time += cost;
                frame_peak = frame_peak.max(cost);
                frames += 1;
                if report_at.elapsed() >= FRAME_REPORT {
                    log::info!(
                        "console: {w}×{h}, {frames} frames in {:?} — {:.1} ms/frame mean, {:.1} ms peak",
                        report_at.elapsed(),
                        frame_time.as_secs_f64() * 1000.0 / f64::from(frames),
                        frame_peak.as_secs_f64() * 1000.0,
                    );
                    (frames, frame_time, frame_peak, report_at) =
                        (0, Duration::ZERO, Duration::ZERO, Instant::now());
                }
                if let Err(e) = s.swap() {
                    // The window went away under us; wait for the next surface.
                    log::warn!("console: {e:#} — dropping the surface");
                    skia = None;
                    egl.release_current();
                    surface = None;
                    window = None;
                }
            }
        }

        if gl_failures >= GL_FAILURE_LIMIT {
            // Same release order as `Cmd::Quit`: the Skia surface, the current binding, then (on
            // return) the EGL surface + window + context drop.
            drop(skia.take());
            if surface.is_some() {
                egl.release_current();
            }
            bail!("GL surface failed {gl_failures} times in a row — giving the screen back");
        }

        // Publish what the console raised.
        while let Some(a) = console.take_action() {
            shared.emit(HostEvent::Action(a));
        }
        let editing = console.editing();
        if editing != was_editing {
            was_editing = editing;
            shared.emit(HostEvent::Editing(editing));
        }
        if store.saved_gen() != saved_gen {
            let (settings, current_gen) = store.snapshot();
            saved_gen = current_gen;
            shared.emit(HostEvent::Settings(Box::new(settings)));
        }
    }
}
