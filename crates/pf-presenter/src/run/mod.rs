//! Session lifecycle: one SDL context on the caller's main thread drives the window,
//! Vulkan presenter, input capture, pumped gamepad service, and the session pump's
//! event/frame channels.
//!
//! Two modes, one loop. **single** (`run_session`) is one `--connect` stream and
//! exits when it ends. **browse** (`run_browse`) idles the console library between
//! streams; overlay actions launch, session end returns to the library.
//!
//! Stdout is the machine interface: `{"ready":true}` after the first presented frame, then
//! once per window while the overlay tier is not Off: `stats: …` (the Advanced Detailed
//! text, lines joined by ` | `) and `stats-json: …` (the snapshot). Logs go to stderr.
//!
//! In-stream chords share Ctrl+Alt+Shift: Q release/engage, M mouse model, D
//! disconnect, S stats tier, V microphone mute.

use crate::input::{Capture, FingerPhase};
use crate::overlay::{
    FrameCtx, Overlay, OverlayAction, OverlayFrame, PointerButton, PointerInput, RingCommand,
    RingFacts, RingInput, SessionPhase,
};
use crate::present_pace::{
    Cadence, CadenceProbe, FrameStore, LatchClock, PresentGate, SourcePacer, MARGIN_MAX_NS,
    MARGIN_STEP_NS,
};
use crate::touch::{Abs, Act};
use crate::vk::{FrameInput, Presented, Presenter};
use anyhow::{Context as _, Result};
use pf_client_core::gamepad::{GamepadService, SelectChord};
use pf_client_core::orchestrate::{emit, SessionLine};
use pf_client_core::session::{self, DecodeFacts, SessionEvent, SessionHandle, SessionParams};
use pf_client_core::trust::{MouseMode, PresentPriority, StatsVerbosity, TouchMode};
use pf_client_core::video::VulkanDecodeDevice;
use pf_client_core::video::{DecodeHealth, DecodedFrame, DecodedImage};
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, Mode};
use punktfunk_core::hud::{self, HudLine, StatsSnapshot};
use punktfunk_core::quic::HdrMeta;
use punktfunk_core::video_fit::{self, VideoFit};
use sdl3::event::{DisplayEvent, Event, WindowEvent};
use sdl3::keyboard::Mod;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod events;
mod pace;
mod shell;
mod stream;

use events::*;
use pace::*;
use shell::*;
use stream::*;

/// [`SessionOpts::on_connected`]: host fingerprint, then Welcome's management-API
/// port (`0` = none advertised).
pub type ConnectedFn = Box<dyn FnMut([u8; 32], u16)>;

pub struct SessionOpts {
    pub window_title: String,
    pub fullscreen: bool,
    /// Desktop top-left; `None` = primary-display center. Shells pass their own window so
    /// the stream opens on the same monitor (fullscreen follows that display).
    pub window_pos: Option<(i32, i32)>,
    /// OSD tier at start; also gates stdout `stats:` lines. Ctrl+Alt+Shift+S cycles live.
    pub stats_verbosity: StatsVerbosity,
    /// Latched per session. A mouse-only client leaves the default and never sees a finger.
    pub touch_mode: TouchMode,
    /// `Capture` (pointer lock + relative) or `Desktop` (uncaptured absolute). Ctrl+Alt+Shift+M
    /// flips it live; hosts without absolute injection (gamescope) stay captured.
    pub mouse_mode: MouseMode,
    pub invert_scroll: bool,
    /// Send system chords (Alt+Tab, Super) to the host while captured. Off keeps them local.
    /// Applies in both mouse models; desktop mode's unlocked pointer clicking another window
    /// is the way back. See [`apply_capture`](shell::apply_capture).
    pub inhibit_shortcuts: bool,
    /// Quick-action ring blob; empty = the platform default ring.
    pub overlay_actions: String,
    /// `Latency` = newest-wins arrival pacing; `Smooth { buffer }` = FIFO one frame per latch
    /// slot. `PUNKTFUNK_PRESENTER=arrival` forces the latency drain without a rebuild.
    pub present_priority: PresentPriority,
    /// Tear-free present (default on). Off asks for a tearing mode; the mode that took is
    /// named in the stats line.
    pub vsync: bool,
    /// Prefer a present mode that drives VRR when the session starts fullscreen.
    pub allow_vrr: bool,
    pub json_status: bool,
    /// Once on `Connected`: host fingerprint and Welcome's management-API port (`0` = none).
    /// This loop stays store-agnostic. The port is the one moment a client has it without mDNS.
    pub on_connected: Option<ConnectedFn>,
    /// `None` is the Skia-free build (stats stay stdout-only). Init failure degrades to `None`
    /// with a warning rather than killing the session. Browse mode requires one.
    pub overlay: Option<Box<dyn Overlay>>,
    /// Starting logical size; `None` = 1280×720. Match-window passes the persisted last size
    /// so the first connect's mode already matches the glass.
    pub window_size: Option<(u32, u32)>,
    /// `Some` = stream mode follows the window: start params use physical pixels, a mid-session
    /// resize sends a debounced `Reconfigure`. The callback gets logical size at each resize-end
    /// for persist. `None` = never auto-resize.
    pub match_window: Option<Box<dyn FnMut(u32, u32)>>,
    /// Multiplier on the window pixel size under Match-window. `> 1` supersamples; `1.0` is
    /// native pixels. See [`punktfunk_core::render_scale`].
    pub render_scale: f64,
    /// Codec per-axis ceiling for the render-scale clamp (4096 for H.264, else 8192).
    pub render_scale_max_dim: u32,
    /// How a frame of another aspect fills the window. The blit and every absolute input
    /// map through the same [`video_fit::place`].
    pub video_fit: VideoFit,
}

pub enum Outcome {
    /// `None` = user quit; `Some` = the reason the pump reported.
    Ended(Option<String>),
    ConnectFailed {
        msg: String,
        trust_rejected: bool,
    },
}

/// Browse-mode overlay action result.
pub enum ActionOutcome {
    Handled,
    /// Launch. Boxed because SessionParams is large next to the unit variants.
    Start(Box<SessionParams>),
    Quit,
}

/// One `--connect` stream; returns when it ends.
pub fn run_session<F>(opts: SessionOpts, build_params: F) -> Result<Outcome>
where
    F: FnOnce(
        &GamepadService,
        Mode,
        Option<HdrMeta>,
        Arc<AtomicBool>,
        Option<VulkanDecodeDevice>,
    ) -> SessionParams,
{
    let mut build = Some(build_params);
    run_inner(
        opts,
        ModeCtl::Single(Box::new(move |gp, native, hdr, fs, vk| {
            (build.take().expect("single build runs once"))(gp, native, hdr, fs, vk)
        })),
    )
    .map(|o| o.expect("single mode always yields an outcome"))
}

/// Console library idles between streams. `on_action` gets every overlay action plus what
/// a launch needs: gamepad service, native display mode, the window display's HDR volume
/// ([`window_display_hdr`]), a fresh `force_software` flag.
pub fn run_browse<F>(opts: SessionOpts, on_action: F) -> Result<()>
where
    F: FnMut(
        OverlayAction,
        &GamepadService,
        Mode,
        Option<HdrMeta>,
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

/// Params builder for the one single-mode session (called once, after setup).
type BuildParams<'a> = Box<
    dyn FnMut(
            &GamepadService,
            Mode,
            Option<HdrMeta>,
            Arc<AtomicBool>,
            Option<VulkanDecodeDevice>,
        ) -> SessionParams
        + 'a,
>;
type OnAction<'a> = Box<
    dyn FnMut(
            OverlayAction,
            &GamepadService,
            Mode,
            Option<HdrMeta>,
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

/// Custom SDL event a decoded frame's arrival pushes (see [`StreamState::new`]).
/// Pure wake-up: the loop drains the frame channel regardless of why it woke.
struct FrameWake;

/// Decoded frame plus when the source cadence says it is due on glass. Due time is
/// from the arrival process the store saw, not from whatever survived it.
struct Paced {
    frame: DecodedFrame,
    /// `session::now_ns` domain (`DecodedFrame::decoded_ns` is the same clock). `0`
    /// under the latency intent, which never asks.
    due_ns: i64,
}

/// One stream session's live state. Created at start, dropped at end; browse cycles
/// several per process.
struct StreamState {
    handle: SessionHandle,
    /// Decoded frames, re-queued by the wake forwarder (newest-wins, like the pump).
    /// The loop drains this, never `handle.frames` — the forwarder is that channel's
    /// one consumer.
    frames: async_channel::Receiver<DecodedFrame>,
    connector: Option<Arc<NativeClient>>,
    capture: Option<Capture>,
    force_software: Arc<AtomicBool>,
    /// User canceled this connect: skip capture/attach on a late `Connected` and
    /// route its end back silently.
    canceled: bool,
    ready_announced: bool,
    mode_line: String,
    /// Settings preset this session resolved; `None` = global defaults, nothing shown.
    preset: Option<String>,
    /// Latch grid the pump's PhaseReports read, written by the 1 Hz present-timing fold.
    /// `None` = the session did not advertise phase lock.
    latch_grid: Option<Arc<session::LatchGrid>>,
    /// Host↔client clock offset (`None` until Connected). Loaded per present so a
    /// mid-stream re-sync keeps e2e honest after an NTP step.
    clock_offset: Option<Arc<std::sync::atomic::AtomicI64>>,
    /// Video-leg e2e in ns, published on every presented frame for the audio plane.
    video_e2e: Option<Arc<std::sync::atomic::AtomicU64>>,
    hdr: bool,
    /// OSD `HDR→SDR (raw)`: this lane showed PQ with no tone-map. Nothing sets it
    /// today — every lane goes through planar CSC. Kept so a future bypass can say so.
    hdr_untonemapped: bool,
    /// Per present: D3D11 import lookup (0 off that lane) and `vkQueueSubmit` wall time,
    /// for the presenter window line.
    win_import_us: Vec<u32>,
    win_submit_us: Vec<u32>,
    /// Per present: the in-flight fence wait, `vkAcquireNextImageKHR`, `vkQueuePresentKHR`.
    win_fence_us: Vec<u32>,
    win_acquire_us: Vec<u32>,
    win_present_us: Vec<u32>,
    /// The overlay window (`NativeClient::hud`) closes here once a second.
    win_start: Instant,
    /// Last closed window, so a tier cycle re-renders at once rather than up to 1 s later.
    last_snap: Option<StatsSnapshot>,
    /// Latest decoder facts from the pump, and the integrity counters as of the last window.
    facts: DecodeFacts,
    health_seen: Option<DecodeHealth>,
    /// Glass-gate force-opens in the last window. The VRR probe trusts only healthy windows.
    last_forced: u32,
    /// Newest-wins under latency, smoothing FIFO under smoothness. A smoothing store
    /// holds decoder-pool frames up to `buffer` deep on top of the depth-2 wake
    /// channels — headroom for 1..=3; deeper must revisit pool sizing.
    store: FrameStore<Paced>,
    /// Panel latch grid (present-wait glass stamps; submit-anchored fallback). Smoothness
    /// slot clock, and the values published to the host-facing `latch_grid`.
    clock: LatchClock,
    /// Plays smoothness frames on the source's cadence, not on arrival. Inert under
    /// latency, which never folds a frame into it.
    pacer: SourcePacer,
    /// Source's nominal frame interval: the negotiated stream mode's refresh, never
    /// the panel's. A 120 fps stream on a 60 Hz panel would otherwise license twice the hold.
    source_interval_ns: i64,
    /// FIFO glass budget (one undisplayed present in flight). Inert off FIFO modes or
    /// without present timing.
    gate: PresentGate,
    /// Variable refresh actually live? Measured from on-glass stamps (no portable query).
    cadence: CadenceProbe,
    /// Display mode's refresh period — the vblank grid presents quantize to when VRR is
    /// off. Not the learned period (see the probe's call site).
    mode_period_ns: u64,
    /// Smoothness slot-pick margin: starts 0 (a fixed lead is display tax), widens
    /// +500 µs per >2-miss window toward 2.5 ms.
    margin_ns: u64,
    /// This window's latch misses (glass later than one panel period past submit plus
    /// the applied lead). Adaptive margin's error signal.
    win_misses: u32,
    win_out_max: usize,
    /// Consecutive on-glass spacings this window, in whole panel periods: `[0, 1, 2, 3, 4, 5+]`.
    /// The mode is the expected step; everything else is judder.
    win_steps: [u32; 6],
    /// Non-blocking presents that came back busy this window: [fence, acquire].
    win_busy: [u32; 2],
    /// What the held frame waits on. The fence paces the loop itself (the presenter waits
    /// it for a millisecond per pass), so the pass turns straight around and drains the
    /// channel first: a newer frame replaces the held one instead of queuing behind it.
    busy_on: crate::vk::BusyOn,
    last_displayed_ns: u64,
    /// Smoothing: the latch slot the last vended frame was aimed at. One present per
    /// slot; a second frame due before the same slot waits for the next.
    last_slot_ns: u64,
    /// The presenter handed the frame back (no swapchain image yet): wake in 1 ms.
    busy_retry: bool,
    /// One-shot log latch: smoothness was requested but PyroWave collapsed the store
    /// to latency (plane-ring retirement assumes newest-wins).
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    pyro_latency_forced: bool,
    /// Hardware-path health: a failure streak (or no import support) demotes the
    /// decoder to software via the shared flag — once per session.
    dmabuf_demoted: bool,
    /// PyroWave present has no demote rung. Warn on the first of a streak; stay quiet
    /// until a present succeeds.
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    pyro_present_warned: bool,
    /// Same latch for the software lane: last rung, so a present failure has nothing
    /// left to demote to.
    cpu_present_warned: bool,
    hw_fails: u32,
    osd: Vec<HudLine>,
    /// Last resize event's stamp. `Some` = pending; the tick fires once ~400 ms pass
    /// with no further size events (never per drag-frame — each switch rebuilds the host).
    resize_pending: Option<Instant>,
    /// When the last `Reconfigure` was sent — ≥ 1 s between requests. The accept ack
    /// round-trips in milliseconds, so this also keeps at most ~one request outstanding.
    resize_sent_at: Option<Instant>,
    /// Last size actually requested. Each distinct size at most once: a rejected size
    /// is not re-asked until it changes, and a host-side rollback cannot loop forever.
    resize_requested: Option<(u32, u32)>,
    /// Connector mode last shown in the HUD/title — a change refreshes both.
    shown_mode: Option<Mode>,
    /// Scrim + spinner. Armed by [`resize_tick`](stream::resize_tick) when it requests a
    /// switch; cleared when a decoded frame reaches the target (or on timeout).
    resize_overlay: ResizeIndicator,
    /// Last presented frame's video dimensions. Touch passthrough maps a finger into
    /// this letterboxed rect; `None` until the first frame, and touches before then drop.
    last_video: Option<(u32, u32)>,
    /// Created with the connector; inert when the host did not negotiate the channel.
    cursor_chan: Option<crate::cursor::CursorChannel>,
    /// Auto-flip fires on changes only, so it never fights a user who chorded away.
    last_hint: Option<bool>,
    /// When `last_hint` last changed; the flip waits out
    /// [`HINT_SETTLE`](events::HINT_SETTLE) from here.
    hint_since: std::time::Instant,
    /// When the user last moved the mouse; the local cursor follows host-driven motion only
    /// after [`FOLLOW_HOST_AFTER`](events::FOLLOW_HOST_AFTER) of stillness.
    last_user_motion: std::time::Instant,
    /// Motion events before this are the echo of a follow-warp.
    warp_echo_until: std::time::Instant,
    /// User flipped the model manually. The standing hint stops driving until the
    /// host's intent next changes (a fresh hint edge clears this and applies).
    hint_override: bool,
    /// Last `client_draws` told to the host; `None` = nothing sent yet. Edge-detected
    /// from the live mouse model so chord, auto-flip, and engage/release share one path.
    sent_client_draws: Option<bool>,
    /// Welcome advert, then every mid-session `AccessUpdate` (latest wins). Default is
    /// full control, permanent — what a host that never sent access decodes to.
    access: pf_client_core::access::SessionAccess,
    /// Transient access toast and when it went up — cleared after
    /// [`ACCESS_NOTICE_S`](shell::ACCESS_NOTICE_S). An access change outranks "click to
    /// capture" for a few seconds.
    session_notice: Option<(String, Instant)>,
    /// Gaming Mode touch-as-mouse: drops leaked Steam Input positions sent as deltas, once.
    touch_mouse: crate::touch::SteamTouchMouse,
    /// Host's pinned fingerprint once connected — the key the pre-fetched host-actions cache uses.
    fp_hex: String,
    native_mode: (u32, u32, u32),
    /// Launch params, kept for codec-fallback re-dial. Clone is at start, so a mid-session
    /// accepted mode switch is not in here — the retry re-reads it from the connector.
    /// The latch grid rides by `Arc` (it is the presenter's). `force_software` does not:
    /// it is a per-session demote latch, and the retry replaces it.
    params: SessionParams,
}

fn run_inner(mut opts: SessionOpts, mut mode: ModeCtl) -> Result<Option<Outcome>> {
    // Before any window exists: unpackaged runs adopt the shell's AppUserModelID so
    // shell⇄session windows group as one taskbar app (MSIX identity wins).
    #[cfg(windows)]
    crate::win32::set_app_user_model_id();
    // This thread presents and forwards input; a late wake is a missed refresh.
    pf_client_core::audio_rt::boost_and_log("presenter");
    sdl3::hint::set("SDL_JOYSTICK_THREAD", "1");
    // Hold Valve HIDAPI off before SDL_Init: the Deck driver clears digital mappings
    // at enumeration. A hint set after `sdl.gamepad()` only detaches a driver that
    // already killed the trackpad-mouse. They are still enabled for an attached session.
    pf_client_core::gamepad::preinit_disable_valve_hidapi();
    // Touch is forwarded as real touch below. Left on, SDL's mouse-from-touch synthesis
    // warps a synthetic mouse; under relative lock that is a large positive delta that
    // walks the host cursor into the corner.
    sdl3::hint::set("SDL_TOUCH_MOUSE_EVENTS", "0");
    // Wayland `app_id` (and X11 WM_CLASS) so compositors match io.unom.Punktfunk.desktop.
    // Without it SDL uses a generic identity and the session window gets the default icon.
    sdl3::hint::set("SDL_APP_ID", "io.unom.Punktfunk");
    // `PUNKTFUNK_DRM_CARD=<n>` → SDL's KMSDRM device index. SDL takes the first card
    // it can open, often the wrong one on a multi-GPU box. Detecting "already mastered"
    // needs the ioctl that taking master is, so this stays an explicit operator choice.
    if let Ok(card) = std::env::var("PUNKTFUNK_DRM_CARD") {
        if card.chars().all(|c| c.is_ascii_digit()) && !card.is_empty() {
            tracing::info!(
                card,
                "PUNKTFUNK_DRM_CARD: pinning SDL's KMSDRM device index"
            );
            sdl3::hint::set("SDL_KMSDRM_DEVICE_INDEX", &card);
        } else {
            tracing::warn!(
                card,
                "PUNKTFUNK_DRM_CARD must be a card NUMBER (e.g. 0) — ignoring"
            );
        }
    }
    let sdl = sdl3::init().context("SDL init")?;
    let video = sdl.video().context("SDL video")?;
    let events = sdl.event().context("SDL events")?;
    events
        .register_custom_event::<FrameWake>()
        .map_err(|e| anyhow::anyhow!("register FrameWake event: {e}"))?;
    let mut window = {
        // Match-window: open at the persisted last size so the first connect's mode
        // matches the glass. 1280×720 is the fallback.
        let (ww, wh) = opts.window_size.unwrap_or((1280, 720));
        let mut b = video.window(&opts.window_title, ww.max(320), wh.max(200));
        match opts.window_pos {
            Some((x, y)) => b.position(x, y),
            None => b.position_centered(),
        };
        // HIGH_PIXEL_DENSITY: backbuffer in the panel's real pixels. Without it SDL
        // leaves a Wayland surface at buffer scale 1, so a fractionally scaled output
        // builds the swapchain in points. The flag only widens `size_in_pixels()`;
        // `size()` stays logical (persisted size and SDL mouse coords).
        b.resizable().vulkan().high_pixel_density();
        if opts.fullscreen {
            b.fullscreen();
        }
        b.build().context("SDL window")?
    };
    // SDL wheel input remains the fallback when native Wayland capture is unavailable.
    let mut scroll_routing = crate::scroll_routing::ScrollRouting::new(&window);
    // Exe-embedded icon onto the title bar/taskbar; a no-op for exes that embed none.
    #[cfg(windows)]
    crate::win32::stamp_window_icon(&window);
    let instance_exts = window
        .vulkan_instance_extensions()
        .map_err(|e| anyhow::anyhow!("vulkan instance extensions: {e}"))?;
    let mut presenter = Presenter::new(
        &window,
        &instance_exts,
        crate::vk::PresentPref {
            vsync: opts.vsync,
            allow_vrr: opts.allow_vrr,
            fullscreen: opts.fullscreen,
            // `vrr_fifo_opt_in` and `fifo_latest_ready` are resolved inside `Presenter::new`.
            // `..Default` keeps this site from breaking when the struct learns another field.
            ..Default::default()
        },
    )
    .context("vulkan presenter")?;
    presenter.set_video_fit(opts.video_fit);
    // A valid black frame immediately — the window is honest while the connect runs.
    presenter.present(&window, FrameInput::Redraw, None)?;

    // `PUNKTFUNK_PRESENTER=arrival` forces the latency drain without a rebuild.
    let arrival_override = std::env::var("PUNKTFUNK_PRESENTER").ok().as_deref() == Some("arrival");
    let present_priority = if arrival_override {
        tracing::info!("PUNKTFUNK_PRESENTER=arrival — presentation pacing disabled");
        PresentPriority::Latency
    } else {
        opts.present_priority
    };
    let pacing_active = !arrival_override;
    let present_debug = std::env::var_os("PUNKTFUNK_PRESENT_DEBUG").is_some();
    // Present completions wake the loop like decoded frames: a glass-gate reopen or a
    // smoothness slot must not wait out the event timeout.
    {
        let sender = events.event_sender();
        presenter.set_present_wake(Box::new(move || {
            let _ = sender.push_custom_event(FrameWake);
        }));
    }
    // Browse is "ready" the moment the library window presents — there may never be a
    // stream. Single mode announces on the first video frame instead.
    if opts.json_status && matches!(mode, ModeCtl::Browse(_)) {
        emit(SessionLine::Ready);
    }

    // Operator preference on top of the display DPI. Read once (a preference, not
    // session state); the DPI part is re-read per frame.
    let osd_scale_pref = std::env::var("PUNKTFUNK_OSD_SCALE")
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(1.0);

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
    let chord_rx = gamepad.chord_events();
    // A Select chord eats the button pressed with Select, so it is only worth claiming where
    // the overlay it drives exists — a build without the console UI leaves A and X to the game.
    gamepad.set_chords_live(overlay.is_some());
    // Ring pad ownership, edge-tracked: open masks the pads (a held trigger is released
    // on the host) and polls them into menu events; close re-adopts them.
    let mut ring_was_open = false;
    // The pad whose Select+A opened the ring; `None` for a keyboard, touch or closed ring.
    let mut ring_opener: Option<u8> = None;
    // Last audio-mute mask drawn and when it changed: a local mute's badge is timed off it.
    let mut audio_mute_seen: u8 = 0;
    let mut audio_mute_at = Instant::now();
    let disconnect_rx = gamepad.disconnect_events();
    let menu_rx = gamepad.menu_events();
    if matches!(mode, ModeCtl::Browse(_)) {
        // Menu mode for the launcher's lifetime (an attached session supersedes translation).
        gamepad.set_menu_mode(true);
    }
    // Gaming Mode's Steam menu / QAM drive the same physical pad we forward, and
    // gamescope never takes our X focus away, so SDL's background-input gate cannot
    // fire there. `None` everywhere else, where window focus is the signal.
    #[cfg(target_os = "linux")]
    let overlay_focus = pf_client_core::overlay_focus::OverlayFocus::start();
    // Window focus and the gamescope overlay OR into one mask pushed on an edge.
    // Kept as separate inputs: either would otherwise clear the other's mask.
    let mut focus_lost = false;
    let mut mask_applied = false;

    // Native display mode — the `0 = native` fallback for the requested stream mode.
    let native = window
        .get_display()
        .and_then(|d| d.get_mode())
        .map(|m| native_mode(m.w, m.h, m.pixel_density, m.refresh_rate))
        .ok()
        // A zero-sized mode is as useless as no mode. Without this filter a display
        // that reports 0×0 streams a 0×0 request.
        .filter(|m: &Mode| m.width > 0 && m.height > 0)
        .unwrap_or(Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        });

    let mut stream: Option<StreamState> = match &mut mode {
        ModeCtl::Single(build) => {
            let force_software = Arc::new(AtomicBool::new(false));
            let mut params = build(
                &gamepad,
                native,
                window_display_hdr(&window),
                force_software.clone(),
                presenter.vulkan_decode(),
            );
            if opts.match_window.is_some() {
                apply_match_window(
                    &mut params,
                    &window,
                    opts.render_scale,
                    opts.render_scale_max_dim,
                );
            }
            Some(StreamState::new(
                params,
                force_software,
                events.event_sender(),
                present_priority,
                native.refresh_hz,
            ))
        }
        ModeCtl::Browse(_) => None,
    };

    let mut event_pump = sdl
        .event_pump()
        .map_err(|e| anyhow::anyhow!("SDL event pump: {e}"))?;
    let mouse = sdl.mouse();

    let mut fullscreen = opts.fullscreen;
    // Latched for the loop's life: `opts` is borrowed mutably for its callbacks at
    // several `apply_capture` sites.
    let inhibit_shortcuts = opts.inhibit_shortcuts;
    let mut stats_verbosity = opts.stats_verbosity;
    let mut overlay_frame: Option<OverlayFrame> = None;
    // SDL text input tracks overlay editing (IME / Steam OSK). Toggled edge-wise —
    // start/stop are not free on Wayland.
    let mut text_input_on = false;
    // Ring Keyboard slot: hold text input on, which summons Steam's OSK under gamescope.
    let mut ring_keyboard = false;
    let mut overlay_damage = OverlayDamage::default();

    let outcome = 'main: loop {
        // Block in SDL's wait: input/window events and decoded frames (FrameWake) all
        // land in this queue. The timeout only bounds stop-flag/pump-tick latency.
        // Smoothness tightens it to the next latch-slot deadline.
        let timeout = stream
            .as_ref()
            .map_or(Duration::from_millis(15), |st| st.wake_timeout());
        let first = event_pump.wait_event_timeout(timeout);
        let mut queued: Vec<Event> = Vec::new();
        if let Some(e) = first {
            queued.push(e);
        }
        while let Some(e) = event_pump.poll_event() {
            queued.push(e);
        }
        scroll_routing.begin(
            stream.as_ref().and_then(|s| s.capture.as_ref()),
            overlay.as_deref(),
        );
        for event in queued {
            // Console UI sees input first: a consumed event never reaches capture/forwarding.
            if let Some(o) = overlay.as_mut() {
                if o.handle_event(&event) {
                    scroll_routing.consumed(&event);
                    continue;
                }
                // Mouse/touch: console hit-tests in its own pixel space. Consumed while
                // the console is up; ignored while streaming (those belong to `Capture`).
                if let Some(input) = overlay_pointer(&event, &window) {
                    if o.handle_pointer(input) {
                        scroll_routing.consumed(&event);
                        continue;
                    }
                }
            }
            match event {
                Event::Quit { .. } => {
                    if let Some(st) = &mut stream {
                        st.request_quit();
                    }
                    break 'main Some(Outcome::Ended(None));
                }
                Event::Window { win_event, .. } => match win_event {
                    WindowEvent::FocusLost => {
                        scroll_routing.focus_lost();
                        if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                            if cap.release(false) {
                                apply_capture(
                                    &mut window,
                                    &mouse,
                                    false,
                                    false,
                                    inhibit_shortcuts,
                                    0,
                                );
                                tracing::info!("focus lost — input released");
                            }
                        }
                        // Controllers go with keyboard and mouse. SDL already stops
                        // delivering presses here, but nothing zeroed what the host still
                        // believes is held — masking flushes it neutral.
                        focus_lost = true;
                    }
                    WindowEvent::FocusGained => {
                        // Unlike capture, the controller mask has no "the user meant it"
                        // variant — it only mirrors who owns the pad — so regaining focus
                        // always lifts its half.
                        focus_lost = false;
                        // An auto-release (Alt-Tab) undoes itself; a chord release stays
                        // until the user opts in. With the ring up the grab waits for its close.
                        if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                            if cap.should_reengage() && cap.engage() && !ring_was_open {
                                apply_capture(
                                    &mut window,
                                    &mouse,
                                    true,
                                    cap.desktop(),
                                    inhibit_shortcuts,
                                    cap.grants(),
                                );
                                tracing::info!("focus gained — input recaptured");
                            }
                        }
                    }
                    WindowEvent::PixelSizeChanged(..) | WindowEvent::Resized(..) => {
                        // A driver that refuses the new size must not end the session.
                        // A refused fullscreen swapchain costs the fullscreen, not the
                        // stream: fall back to the geometry that was already working.
                        // A windowed failure still propagates — no smaller state to fall back to.
                        if let Err(e) = presenter.recreate_swapchain(&window) {
                            if !fullscreen {
                                return Err(e);
                            }
                            tracing::warn!(
                                error = format!("{e:#}"),
                                "swapchain recreate failed — leaving fullscreen"
                            );
                            fullscreen = false;
                            if let Err(e) = window.set_fullscreen(false) {
                                tracing::warn!(error = %e, "fullscreen exit failed");
                            }
                            continue;
                        }
                        presenter.present(&window, FrameInput::Redraw, overlay_frame.as_ref())?;
                        // Match-window: restamp the debounce. The request fires once
                        // ~400 ms pass with no further size events, never per drag-frame.
                        if opts.match_window.is_some() {
                            if let Some(st) = stream.as_mut() {
                                st.resize_pending = Some(Instant::now());
                            }
                        }
                    }
                    // Dragged to another monitor: latch grid and VRR verdict belong to
                    // the old panel. A 60 Hz-seeded clock must not keep pacing a 144 Hz panel.
                    WindowEvent::DisplayChanged(..) => {
                        if let Some(st) = stream.as_mut() {
                            st.relearn_grid(&window);
                        }
                    }
                    WindowEvent::Exposed => {
                        presenter.present(&window, FrameInput::Redraw, overlay_frame.as_ref())?;
                    }
                    _ => {}
                },
                // The panel's rate changed under the window (60 ↔ 165 Hz in the OS
                // settings): no window event fires, and the grid describes the old rate.
                Event::Display {
                    display_event:
                        DisplayEvent::CurrentModeChanged | DisplayEvent::DesktopModeChanged,
                    display,
                    ..
                } if window.get_display().is_ok_and(|d| d == display) => {
                    if let Some(st) = stream.as_mut() {
                        st.relearn_grid(&window);
                    }
                }
                // Windows never auto-repeats injected input, so a held key needs these.
                // Chords and toggles below fire on the first press only.
                Event::KeyDown {
                    scancode: Some(sc),
                    repeat: true,
                    ..
                } => {
                    if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                        cap.on_key_repeat(sc);
                    }
                }
                Event::KeyDown {
                    keycode,
                    scancode: Some(sc),
                    keymod,
                    repeat: false,
                    ..
                } => {
                    let chord = keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD)
                        && keymod.intersects(Mod::LALTMOD | Mod::RALTMOD)
                        && keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD);
                    use sdl3::keyboard::{Keycode, Scancode};
                    // The letter the layout prints on the key (AZERTY's Q sits on Scancode::A),
                    // or the physical position for a layout that has no such letter.
                    let key = |k: Keycode, s: Scancode| keycode == Some(k) || sc == s;
                    if chord && key(Keycode::Q, Scancode::Q) {
                        if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                            if cap.captured() {
                                cap.release(true);
                                apply_capture(
                                    &mut window,
                                    &mouse,
                                    false,
                                    false,
                                    inhibit_shortcuts,
                                    0,
                                );
                            } else if cap.engage() {
                                apply_capture(
                                    &mut window,
                                    &mouse,
                                    true,
                                    cap.desktop(),
                                    inhibit_shortcuts,
                                    cap.grants(),
                                );
                            }
                            tracing::info!(captured = cap.captured(), "chord: release/engage");
                        }
                        continue;
                    }
                    // Mouse model flip. Applies immediately when engaged; a released
                    // stream just changes what the next engage does.
                    if chord && key(Keycode::M, Scancode::M) {
                        if let Some(st) = stream.as_mut() {
                            let mut flipped = false;
                            if let Some(cap) = st.capture.as_mut() {
                                match cap.toggle_desktop() {
                                    Some(desktop) => {
                                        if cap.captured() {
                                            apply_capture(
                                                &mut window,
                                                &mouse,
                                                true,
                                                desktop,
                                                inhibit_shortcuts,
                                                cap.grants(),
                                            );
                                        }
                                        flipped = true;
                                        tracing::info!(desktop, "chord: mouse mode");
                                    }
                                    None => tracing::info!(
                                        "chord: mouse mode — host has no absolute pointer \
                                         (gamescope), staying captured"
                                    ),
                                }
                            }
                            // A manual flip outranks the standing hint until the host's
                            // intent next changes (the hint edge clears this).
                            if flipped {
                                st.hint_override = true;
                            }
                        }
                        continue;
                    }
                    if chord && key(Keycode::D, Scancode::D) {
                        if let Some(st) = &mut stream {
                            tracing::info!("chord: disconnect");
                            st.request_quit();
                            apply_capture(&mut window, &mouse, false, false, inhibit_shortcuts, 0);
                        }
                        continue;
                    }
                    if chord && key(Keycode::S, Scancode::S) {
                        bump_stats_tier(&mut stats_verbosity, &mut stream);
                        tracing::info!(tier = ?stats_verbosity, "chord: stats verbosity");
                        continue;
                    }
                    // Quick-action ring at the window centre (a locked pointer has no
                    // position worth opening at).
                    if chord && key(Keycode::O, Scancode::O) {
                        if let (Some(o), true) = (overlay.as_mut(), stream.is_some()) {
                            let (pw, ph) = window.size_in_pixels();
                            o.ring_input(RingInput::Toggle {
                                x: pw as f32 / 2.0,
                                y: ph as f32 / 2.0,
                            });
                        }
                        continue;
                    }
                    // Mic mute — per session, never persisted. The uplink keeps running;
                    // only sending stops. A session with no mic says so instead of
                    // swallowing the chord.
                    if chord && key(Keycode::V, Scancode::V) {
                        if let Some(st) = &stream {
                            match st.handle.mic.toggle() {
                                Some(muted) => tracing::info!(muted, "chord: microphone mute"),
                                None => tracing::info!(
                                    "chord: microphone mute — this session streams no \
                                     microphone (turn it on in Settings)"
                                ),
                            }
                        }
                        continue;
                    }
                    // F11 or Alt+Enter (some Fn layers send a media key for plain F11).
                    let alt_enter =
                        sc == Scancode::Return && keymod.intersects(Mod::LALTMOD | Mod::RALTMOD);
                    if sc == Scancode::F11 || alt_enter {
                        fullscreen = !fullscreen;
                        tracing::debug!(fullscreen, "fullscreen toggle");
                        if let Err(e) = window.set_fullscreen(fullscreen) {
                            tracing::warn!(error = %e, fullscreen, "fullscreen toggle failed");
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
                Event::MouseMotion {
                    x, y, xrel, yrel, ..
                } => {
                    if let Some(st) = stream.as_mut() {
                        let video = st.last_video;
                        // The echo of our own follow-warp is not the user moving.
                        if Instant::now() >= st.warp_echo_until {
                            st.last_user_motion = Instant::now();
                        }
                        if let Some(cap) = st.capture.as_mut() {
                            if cap.desktop() {
                                // Desktop model: window position through the placement.
                                // Before the first decoded frame there is nothing to map
                                // onto — dropped, like touch.
                                if let Some(video) = video {
                                    let (lw, lh) = window.size();
                                    let nx = x / lw.max(1) as f32;
                                    let ny = y / lh.max(1) as f32;
                                    cap.on_motion_abs(finger_to_frame(
                                        opts.video_fit,
                                        window.size_in_pixels(),
                                        video,
                                        nx,
                                        ny,
                                    ));
                                }
                            } else if st.touch_mouse.leaks(xrel, yrel) {
                                // Gaming Mode touch-as-mouse: a leaked position, not a
                                // delta — dropped, and said once.
                                if st.touch_mouse.take_notice() {
                                    tracing::warn!(
                                        xrel,
                                        yrel,
                                        "Steam Input is replaying the touchscreen as a mouse — \
                                         dropping the leaked positions"
                                    );
                                    st.session_notice = Some((
                                        "Steam Input is sending the touchscreen as a mouse — \
                                         pick the Punktfunk controller layout for touch"
                                            .into(),
                                        Instant::now(),
                                    ));
                                }
                            } else {
                                cap.on_motion(xrel, yrel);
                            }
                        }
                    }
                }
                Event::MouseButtonDown { mouse_btn, .. } => {
                    if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                        if !cap.captured() {
                            // The engaging click is not forwarded. `engage` refuses when
                            // access covers neither pointer nor keyboard — the click then
                            // does nothing.
                            if cap.engage() {
                                apply_capture(
                                    &mut window,
                                    &mouse,
                                    true,
                                    cap.desktop(),
                                    inhibit_shortcuts,
                                    cap.grants(),
                                );
                            }
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
                    // The overlay consumes SDL wheels before native/fallback routing.
                    scroll_routing.wheel(stream.as_mut().and_then(|s| s.capture.as_mut()), x, y);
                }
                // Touchscreen fingers → the session's touch model. `x`/`y` are
                // window-normalized; only DIRECT devices (an INDIRECT trackpad drives
                // the mouse). A three-finger tap returns `cycle` → bump the stats tier.
                Event::FingerDown {
                    touch_id,
                    finger_id,
                    x,
                    y,
                    timestamp,
                    ..
                } => {
                    if is_direct_touch(touch_id) {
                        if let Some(st) = stream.as_mut() {
                            if !st.touch_mouse.fingers_seen {
                                tracing::info!(
                                    touch_id,
                                    "first touchscreen finger: direct touch reaches the client"
                                );
                            }
                            st.touch_mouse.fingers_seen = true;
                        }
                        if ring_finger(&mut overlay, &window, FingerPhase::Down, x, y) {
                            continue;
                        }
                        for act in dispatch_finger(
                            FingerPhase::Down,
                            &window,
                            &mut stream,
                            finger_id,
                            x,
                            y,
                            timestamp,
                            opts.video_fit,
                        ) {
                            on_touch_act(act, &mut stats_verbosity, &mut stream, &mut overlay);
                        }
                    } else if let Some(st) = stream.as_mut() {
                        // A finger from a device SDL does not call a touchscreen: ignored,
                        // and said once — otherwise "touch arrived and was thrown away"
                        // is indistinguishable from "no touch arrived".
                        if !st.touch_mouse.indirect_seen {
                            st.touch_mouse.indirect_seen = true;
                            tracing::info!(
                                touch_id,
                                "finger from a non-direct touch device — ignored (a trackpad \
                                 drives the mouse)"
                            );
                        }
                    }
                }
                Event::FingerMotion {
                    touch_id,
                    finger_id,
                    x,
                    y,
                    timestamp,
                    ..
                } => {
                    if is_direct_touch(touch_id) {
                        if ring_finger(&mut overlay, &window, FingerPhase::Move, x, y) {
                            continue;
                        }
                        for act in dispatch_finger(
                            FingerPhase::Move,
                            &window,
                            &mut stream,
                            finger_id,
                            x,
                            y,
                            timestamp,
                            opts.video_fit,
                        ) {
                            on_touch_act(act, &mut stats_verbosity, &mut stream, &mut overlay);
                        }
                    }
                }
                Event::FingerUp {
                    touch_id,
                    finger_id,
                    x,
                    y,
                    timestamp,
                    ..
                } => {
                    if is_direct_touch(touch_id) {
                        // The lift also reaches the engine, so it never keeps a finger
                        // that is gone.
                        ring_finger(&mut overlay, &window, FingerPhase::Up, x, y);
                        for act in dispatch_finger(
                            FingerPhase::Up,
                            &window,
                            &mut stream,
                            finger_id,
                            x,
                            y,
                            timestamp,
                            opts.video_fit,
                        ) {
                            on_touch_act(act, &mut stats_verbosity, &mut stream, &mut overlay);
                        }
                    }
                }
                // FrameWake (and any other user event): pure wake-up — the frame drain
                // runs this iteration either way.
                Event::User { .. } => {}
                other => pump.handle_event(other),
            }
        }
        // Native events forward only when capture owns the entire SDL batch.
        scroll_routing.finish(
            stream.as_mut().and_then(|s| s.capture.as_mut()),
            overlay.as_deref(),
        );
        // Who owns the pad: capture, window focus, and Gaming Mode's overlay signal.
        // Edge-triggered so an open QAM does not re-flush the pads every iteration.
        #[cfg(target_os = "linux")]
        let overlay_now = overlay_focus.as_ref().is_some_and(|of| of.is_open());
        #[cfg(not(target_os = "linux"))]
        let overlay_now = false;
        // Remembered, not applied: the ring is a third owner of this same mask and is only
        // known further down. Applying here too gave one boolean two latches, and whichever
        // fell last unmasked the pads while the other still wanted them masked.
        let capture_active = stream
            .as_ref()
            .and_then(|st| st.capture.as_ref())
            .map(Capture::captured);
        let want_mask_ui = ui_wants_pad_mask(focus_lost, overlay_now, capture_active);
        pump.tick();
        // One coalesced MouseMove per iteration — pure motion must reach the host
        // without waiting for a click/key to flush it.
        if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
            cap.flush_motion();
        }
        // Drain forwarded cursor shape/state and drive the local OS cursor — only
        // meaningful in the desktop mouse model (capture's relative lock hides it).
        if let Some(st) = stream.as_mut() {
            // Host-framebuffer px → cursor-surface px: the placement scale (physical px per
            // host px) over what the backend scales a cursor by itself, so the pointer is the
            // size it has in the picture, as on Apple. Stretch keeps the shape undistorted.
            let cursor_scale = st.last_video.map_or(1.0, |video| {
                let p = video_fit::place(opts.video_fit, window.size_in_pixels(), video);
                p.scale_x.min(p.scale_y) as f32 / cursor_surface_scale(&window)
            });
            if let (Some(chan), Some(c)) = (st.cursor_chan.as_mut(), st.connector.as_ref()) {
                let desktop_active = st
                    .capture
                    .as_ref()
                    .is_some_and(|cap| cap.captured() && cap.desktop());
                chan.pump(c, &mouse, desktop_active, cursor_scale);
                // We draw the pointer while released (a released cursor over a composited one is
                // a frozen twin), in desktop mode, or relative on the host's hint: only the state
                // it keeps forwarding can clear the hint. Without the pointer grant the host
                // pointer is someone else's, so the host draws it.
                let hint_relative = !st.hint_override && st.last_hint == Some(true);
                let client_draws = match st.capture.as_ref() {
                    Some(cap) => {
                        (!cap.captured() || cap.desktop() || hint_relative)
                            && cap.grants() & punktfunk_core::quic::GRANT_POINTER != 0
                    }
                    None => true,
                };
                if chan.negotiated() && st.sent_client_draws != Some(client_draws) {
                    st.sent_client_draws = Some(client_draws);
                    let _ = c.set_cursor_render(client_draws);
                }
            }
            // Host-driven mode flip: `relative_hint` set = run captured relative; clear
            // = return to absolute. Edge-triggered so a manual chord is not fought: the
            // override latch holds until the host's intent next changes. The hint must hold
            // [`HINT_SETTLE`] with no button down (Windows hides the pointer for a click or a
            // keystroke), and a grab needs the pointer over this window.
            let hint_state = st.cursor_chan.as_ref().and_then(|ch| ch.state());
            if let Some(hs) = hint_state {
                let hint = hs.relative_hint();
                if st.last_hint != Some(hint) {
                    st.last_hint = Some(hint);
                    st.hint_override = false;
                    st.hint_since = std::time::Instant::now();
                }
                if !st.hint_override && st.hint_since.elapsed() >= HINT_SETTLE {
                    let video = st.last_video;
                    let over_us = !hint || mouse.focused_window_id() == Some(window.id());
                    if let Some(cap) = st.capture.as_mut() {
                        if cap.captured()
                            && !cap.buttons_held()
                            && over_us
                            && !ring_was_open
                            && cap.set_desktop(!hint)
                        {
                            apply_capture(
                                &mut window,
                                &mouse,
                                true,
                                cap.desktop(),
                                inhibit_shortcuts,
                                cap.grants(),
                            );
                            if cap.desktop() {
                                // Reappear where the host last had the pointer so the
                                // hand-back is seamless.
                                if let Some(video) = video {
                                    let (wx, wy) = content_to_window(
                                        opts.video_fit,
                                        window.size(),
                                        window.size_in_pixels(),
                                        video,
                                        hs.x,
                                        hs.y,
                                    );
                                    mouse.warp_mouse_in_window(&window, wx, wy);
                                }
                            }
                            tracing::info!(
                                desktop = cap.desktop(),
                                "host cursor hint: mouse model flipped"
                            );
                        }
                    }
                }
                // Something else moved the pointer we draw (controller mouse, a trackpad
                // gesture, an app warping it): once the user's own mouse has been still for
                // longer than a round trip, the local cursor goes where the host put it.
                let over_us = mouse.focused_window_id() == Some(window.id());
                let still = st.last_user_motion.elapsed() >= FOLLOW_HOST_AFTER;
                if let (Some(cap), Some(video)) = (st.capture.as_mut(), st.last_video) {
                    let drifted = cap.last_abs().is_some_and(|(x, y)| {
                        (x - hs.x).abs() > FOLLOW_SLACK_PX || (y - hs.y).abs() > FOLLOW_SLACK_PX
                    });
                    if cap.captured()
                        && cap.desktop()
                        && hs.visible()
                        && over_us
                        && still
                        && drifted
                    {
                        let (wx, wy) = content_to_window(
                            opts.video_fit,
                            window.size(),
                            window.size_in_pixels(),
                            video,
                            hs.x,
                            hs.y,
                        );
                        st.warp_echo_until = Instant::now() + WARP_ECHO;
                        mouse.warp_mouse_in_window(&window, wx, wy);
                        cap.followed_host((hs.x, hs.y));
                    }
                }
            }
        }

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

        // Select chords on a pad. `Select+A` puts the ring at the window centre, where its
        // highlight starts on the centre so `Select+A` then `A` opens the sheet; `Select+X`
        // steps the stats tier, the same move the keyboard chord and the dial's own slot make.
        while let Ok((pad, chord)) = chord_rx.try_recv() {
            if overlay.is_none() || stream.is_none() {
                continue;
            }
            match chord {
                SelectChord::Ring => {
                    if let Some(o) = overlay.as_mut() {
                        ring_opener = Some(pad);
                        let (pw, ph) = window.size_in_pixels();
                        o.ring_input(RingInput::Toggle {
                            x: pw as f32 / 2.0,
                            y: ph as f32 / 2.0,
                        });
                    }
                }
                SelectChord::Stats => {
                    bump_stats_tier(&mut stats_verbosity, &mut stream);
                    tracing::info!(tier = ?stats_verbosity, "chord: stats verbosity");
                }
            }
        }
        // While the ring is up, or the console holds a launch over the live stream, the
        // pad belongs to the overlay: masked off the wire, polled into menu events. The
        // three gates that keep pad input off client UI flip together.
        let ring_open = stream.is_some()
            && overlay
                .as_ref()
                .is_some_and(|o| o.ring_open() || o.holds_stream());
        if ring_open != ring_was_open {
            ring_was_open = ring_open;
            gamepad.set_ring_nav(ring_open);
            if !ring_open {
                ring_opener = None;
            }
            // The ring eats every pointer event, so a button already down would stay pressed
            // on the host without a flush. It also needs a pointer to aim with: under a lock
            // the cursor is hidden and every event carries the position the lock froze, so
            // capture hands the local one back while it is up and takes the window on close.
            if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                if ring_open {
                    cap.flush_held();
                }
                if cap.captured() {
                    let (on, desktop, grants) = if ring_open {
                        (false, false, 0)
                    } else {
                        (true, cap.desktop(), cap.grants())
                    };
                    apply_capture(&mut window, &mouse, on, desktop, inhibit_shortcuts, grants);
                }
            }
        }
        // One owner for the mask: any gate that wants the pads keeps them masked.
        let want_mask = want_mask_ui || ring_open;
        if want_mask != mask_applied {
            mask_applied = want_mask;
            gamepad.set_masked(want_mask);
        }
        if ring_open {
            while let Ok(ev) = menu_rx.try_recv() {
                if let Some(o) = overlay.as_mut() {
                    o.handle_menu(ev);
                }
            }
        }

        // Controller escape chord: release capture and leave fullscreen. Gamescope has
        // nothing to release into and no pointer to click back with, while a release masks
        // the pads — there the chord only starts the disconnect hold.
        while escape_rx.try_recv().is_ok() {
            if in_gamescope() {
                continue;
            }
            if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                if cap.release(true) {
                    apply_capture(&mut window, &mouse, false, false, inhibit_shortcuts, 0);
                }
            }
            if fullscreen && !opts.fullscreen {
                fullscreen = false;
                let _ = window.set_fullscreen(false);
            }
        }
        // Escape chord held past the threshold: the controller's disconnect.
        if disconnect_rx.try_recv().is_ok() {
            if let Some(st) = &mut stream {
                tracing::info!("controller chord: disconnect");
                st.request_quit();
                apply_capture(&mut window, &mouse, false, false, inhibit_shortcuts, 0);
            }
        }

        if let ModeCtl::Browse(on_action) = &mut mode {
            // Menu events flow while no stream is engaged — including a connect in
            // flight, so B can cancel the dial. Once attached, the worker forwards raw
            // input instead.
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
                    // The console already toasted "Link copied"; a clipboard SDL refuses
                    // is a log line, not a contradiction of the toast.
                    OverlayAction::CopyText(text) => {
                        if let Err(e) = video.clipboard().set_clipboard_text(&text) {
                            tracing::warn!(error = %e, "copying to the clipboard");
                        }
                    }
                    action => {
                        let force_software = Arc::new(AtomicBool::new(false));
                        match on_action(
                            action,
                            &gamepad,
                            native,
                            window_display_hdr(&window),
                            force_software.clone(),
                            presenter.vulkan_decode(),
                        ) {
                            ActionOutcome::Handled => {}
                            ActionOutcome::Start(mut params) => {
                                if opts.match_window.is_some() {
                                    apply_match_window(
                                        &mut params,
                                        &window,
                                        opts.render_scale,
                                        opts.render_scale_max_dim,
                                    );
                                }
                                // Adopt the tier this launch resolved. The console outlives
                                // every stream. Not in `StreamState::new`: a codec-fallback
                                // retry rebuilds from a clone of these params and would snap
                                // the overlay back, undoing a chord the user had just made.
                                stats_verbosity = params.stats_verbosity;
                                // A live pump here would be detached by the assignment —
                                // `StreamState` has no `Drop`, so its thread would keep
                                // decoding onto the shared Vulkan device. Every other
                                // replacement site takes-and-shuts-down; so does this one.
                                if let Some(prev) = stream.take() {
                                    tracing::warn!(
                                        "launch while a session was still attached — \
                                         stopping it first"
                                    );
                                    prev.shutdown();
                                }
                                stream = Some(StreamState::new(
                                    *params,
                                    force_software,
                                    events.event_sender(),
                                    present_priority,
                                    native.refresh_hz,
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

        // `stream` may become None mid-drain (browse-mode session end) — re-borrow each
        // event and stop draining on the terminal ones.
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
                        // now; the stop flag (already set) ends the pump without engaging.
                        c.disconnect_quit();
                        continue;
                    }
                    st.mode_line = format!("{}×{}@{}", m.width, m.height, m.refresh_hz);
                    st.native_mode = (m.width, m.height, m.refresh_hz);
                    st.fp_hex = pf_client_core::trust::hex(&fingerprint);
                    // Pre-fetch the ring's host-action slots here, never when it opens.
                    let host_addr = st.params.host.clone();
                    pf_client_core::host_actions::refresh(&host_addr, c.mgmt_port(), &st.fp_hex);
                    // The resolved rate — a `0 = native` request becomes a real number
                    // here, last moment before frames start arriving.
                    st.source_interval_ns = frame_interval_ns(m.refresh_hz, native.refresh_hz);
                    tracing::info!(mode = %st.mode_line, "connected");
                    // Which touch devices SDL sees. Under gamescope this is the tell
                    // for whether Steam Input hands the touchscreen through as touch:
                    // no DIRECT device, no twist can arrive.
                    tracing::info!(
                        devices = ?touch_devices(),
                        gamescope = in_gamescope(),
                        "touch devices"
                    );
                    window
                        .set_title(&format!("{} · {}", opts.window_title, st.mode_line))
                        .ok();
                    gamepad.attach(c.clone());
                    st.clock_offset = Some(c.clock_offset_shared());
                    st.video_e2e = Some(c.video_e2e_shared());
                    // gamescope's EIS grants only a relative pointer — absolute would be
                    // dropped, so desktop mode is pinned off. Auto (a host that never
                    // said) stays allowed.
                    let abs_ok = c.resolved_compositor != CompositorPref::Gamescope;
                    if opts.mouse_mode == MouseMode::Desktop && !abs_ok {
                        tracing::info!(
                            "desktop mouse mode unavailable on a gamescope host \
                             (relative-only input) — using capture"
                        );
                    }
                    // Access off the Welcome. The pump's Access event lands in this drain,
                    // but capture below must be built gated, not re-gated a beat later.
                    st.access = pf_client_core::access::SessionAccess::from_connector(&c);
                    // Passthrough needs a host that injects touch. Without the bit every
                    // contact would vanish with no error, so the session runs the trackpad
                    // model and the notice says so.
                    let touch_mode = if opts.touch_mode == TouchMode::Touch
                        && c.host_caps2() & punktfunk_core::quic::HOST_CAP2_TOUCH == 0
                    {
                        st.session_notice = Some((
                            "This host does not accept touch — using the trackpad model".into(),
                            Instant::now(),
                        ));
                        TouchMode::Trackpad
                    } else {
                        opts.touch_mode
                    };
                    let mut cap = Capture::new(
                        c.clone(),
                        touch_mode,
                        opts.invert_scroll,
                        opts.mouse_mode,
                        abs_ok,
                        st.access.grants,
                    );
                    // Capture engages when the stream starts unless access covers neither
                    // pointer nor keyboard, where `engage` refuses and the pointer stays free.
                    if cap.engage() {
                        apply_capture(
                            &mut window,
                            &mouse,
                            true,
                            cap.desktop(),
                            inhibit_shortcuts,
                            cap.grants(),
                        );
                    }
                    st.capture = Some(cap);
                    st.cursor_chan = Some(crate::cursor::CursorChannel::new(&c));
                    // Read the mgmt port before `c` is moved into `st` — the Welcome's
                    // library address, which the binary persists so it survives without mDNS.
                    let mgmt_port = c.mgmt_port();
                    st.connector = Some(c);
                    if let Some(f) = opts.on_connected.as_mut() {
                        f(fingerprint, mgmt_port);
                    }
                    if let Some(o) = overlay.as_mut() {
                        o.session_phase(SessionPhase::Streaming);
                    }
                }
                SessionEvent::DecodeFacts(f) => st.facts = f,
                // Welcome advert first, then every mid-session AccessUpdate. Re-gate live
                // capture: a removed POINTER/KEYBOARD bit releases the lock it backed;
                // with neither class left the capture drops (auto-release, so a later
                // re-grant re-engages on click).
                SessionEvent::Notice(n) => {
                    st.session_notice = Some((n, Instant::now()));
                }
                SessionEvent::Access { access, notice } => {
                    st.access = access;
                    if let Some(n) = notice {
                        tracing::info!(notice = %n, "session access changed");
                        st.session_notice = Some((n, Instant::now()));
                    }
                    if let Some(cap) = st.capture.as_mut() {
                        cap.set_grants(access.grants);
                        if cap.captured() {
                            // With the ring up the pointer stays the ring's; its close re-applies.
                            if cap.can_capture() && !ring_was_open {
                                apply_capture(
                                    &mut window,
                                    &mouse,
                                    true,
                                    cap.desktop(),
                                    inhibit_shortcuts,
                                    cap.grants(),
                                );
                            } else if !cap.can_capture() {
                                cap.release(false);
                                apply_capture(
                                    &mut window,
                                    &mouse,
                                    false,
                                    false,
                                    inhibit_shortcuts,
                                    0,
                                );
                            }
                        }
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
                        apply_capture(&mut window, &mouse, false, false, inhibit_shortcuts, 0);
                        if let Some(o) = overlay.as_mut() {
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
                    apply_capture(&mut window, &mouse, false, false, inhibit_shortcuts, 0);
                    match &mode {
                        ModeCtl::Single(_) => break 'main Some(Outcome::Ended(reason)),
                        ModeCtl::Browse(_) => {
                            window.set_title(&opts.window_title).ok();
                            let canceled = st.canceled;
                            if let Some(st) = stream.take() {
                                st.shutdown();
                            }
                            if let Some(o) = overlay.as_mut() {
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
                // The negotiated codec ran out of decode rungs: re-dial the same host
                // with that codec removed from advertised caps. The pump left nothing of
                // its own running before sending this, so this is a clean start, not an
                // overlap. Applies in both modes — single has no console to fall back to.
                SessionEvent::CodecFallback {
                    exclude_codecs,
                    retry_caps,
                    msg,
                } => {
                    tracing::warn!(
                        %msg,
                        exclude_codecs,
                        retry_caps,
                        "decode ladder exhausted — reconnecting with reduced codec caps"
                    );
                    gamepad.detach();
                    if let Some(cap) = &mut st.capture {
                        cap.release(true);
                    }
                    apply_capture(&mut window, &mouse, false, false, inhibit_shortcuts, 0);
                    // Widen the exclusion rather than replace it: a second fallback must
                    // not re-offer what the first already ruled out.
                    let mut params = st.params.clone();
                    params.exclude_codecs |= exclude_codecs;
                    // The mode this session ended on, not the one it dialled with: a
                    // mid-session `Reconfigure` lives only in the connector, and
                    // `st.params` is a launch clone.
                    if let Some(c) = &st.connector {
                        params.mode = c.mode();
                    }
                    // Then the window follower on top, so a retry lands on the size the
                    // window is now.
                    if opts.match_window.is_some() {
                        apply_match_window(
                            &mut params,
                            &window,
                            opts.render_scale,
                            opts.render_scale_max_dim,
                        );
                    }
                    // A fresh demote flag, like `ActionOutcome::Start` — never the old
                    // session's. Inheriting it would open a software decoder on good hardware.
                    let force_software = Arc::new(AtomicBool::new(false));
                    params.force_software = force_software.clone();
                    // `params.launch` rides along verbatim. Dropping it would miss
                    // `pf-vdisplay`'s reuse key (it includes the launch command) and
                    // orphan the running game inside the lingering display. A `gog:`/
                    // `custom:` target may start a second copy; Steam/Epic URIs dedupe.
                    if let Some(st) = stream.take() {
                        st.shutdown();
                    }
                    if let Some(o) = overlay.as_mut() {
                        o.session_phase(SessionPhase::Reconnecting(&msg));
                    }
                    stream = Some(StreamState::new(
                        params,
                        force_software,
                        events.event_sender(),
                        present_priority,
                        native.refresh_hz,
                    ));
                    break;
                }
            }
        }

        // HUD/title follow the live mode slot on any accepted switch — also when the
        // match-window follower is off (another trigger, or a host-side rollback).
        if let Some(st) = stream.as_mut() {
            hud_mode_tick(st, &mut window, &opts.window_title);
        }
        if let Some(persist) = opts.match_window.as_mut() {
            if let Some(st) = stream.as_mut() {
                resize_tick(
                    st,
                    &mut window,
                    persist.as_mut(),
                    opts.render_scale,
                    opts.render_scale_max_dim,
                );
            }
        }
        // A switch the host rejected/capped never delivers the exact target frame —
        // drop the scrim so it cannot linger.
        if let Some(st) = stream.as_mut() {
            st.resize_overlay.tick(Instant::now());
        }
        // Touch long-press: a still finger raises no SDL event, so the gesture engine
        // needs the clock — SDL ticks, the millisecond base the finger timestamps use.
        if let Some(cap) = stream.as_mut().and_then(|st| st.capture.as_mut()) {
            cap.tick(sdl3::timer::ticks() as f64);
        }
        let mut ring_cmds = Vec::new();
        if let (Some(o), true) = (overlay.as_mut(), stream.is_some()) {
            while let Some(cmd) = o.take_ring_command() {
                ring_cmds.push(cmd);
            }
        }
        for cmd in ring_cmds {
            tracing::info!(?cmd, "ring");
            match cmd {
                RingCommand::CycleStats => {
                    bump_stats_tier(&mut stats_verbosity, &mut stream);
                }
                RingCommand::Keyboard => ring_keyboard = !ring_keyboard,
                RingCommand::TogglePadMouse => {
                    if let Some(c) = stream.as_ref().and_then(|st| st.connector.as_ref()) {
                        toggle_pad_mouse(c, ring_opener);
                    }
                }
                RingCommand::ToggleStreamMute => {
                    if let Some(c) = stream.as_ref().and_then(|st| st.connector.as_ref()) {
                        let on = c.audio_mute() & punktfunk_core::client::AUDIO_MUTE_LOCAL != 0;
                        c.set_audio_muted(!on);
                    }
                }
                // The pad worker owns the wire index and the owed release, so this one is
                // the service's, not `ring_command`'s.
                RingCommand::TapButton(bit) => gamepad.tap_button(bit),
                other => {
                    if let Some(st) = stream.as_mut() {
                        ring_command(other, st, &mut window, &mouse, inhibit_shortcuts);
                    }
                }
            }
        }

        if let Some(st) = stream.as_mut() {
            if st
                .session_notice
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(ACCESS_NOTICE_S))
            {
                st.session_notice = None;
            }
        }

        if let Some(o) = overlay.as_mut() {
            let (pw, ph) = window.size_in_pixels();
            let (stats, hint) = match &stream {
                Some(st) if st.connector.is_some() => {
                    // No "click to capture" over a session with nothing to capture for.
                    let hint = match &st.capture {
                        Some(cap) if !cap.captured() && cap.can_capture() => {
                            Some(if gamepad.active().is_some() {
                                HINT_WITH_PAD
                            } else {
                                HINT_KEYBOARD
                            })
                        }
                        _ => None,
                    };
                    (
                        (stats_verbosity != StatsVerbosity::Off && !st.osd.is_empty())
                            .then_some(st.osd.as_slice()),
                        hint,
                    )
                }
                _ => (None, None),
            };
            // Access chip: a standing pill in the stats overlay family. A pill that never
            // goes away is chrome, so it rides the stats tier. `None` for a full-control
            // permanent session — what a host that never sent access looks like.
            let access_chip = match &stream {
                Some(st) if st.connector.is_some() && stats_verbosity != StatsVerbosity::Off => {
                    st.access.chip_text(Instant::now())
                }
                _ => None,
            };
            let session_notice = stream
                .as_ref()
                .filter(|st| st.connector.is_some())
                .and_then(|st| st.session_notice.as_ref().map(|(n, _)| n.as_str()));
            let pad = gamepad.active();
            let pads = gamepad.pads();
            let resizing = stream
                .as_ref()
                .is_some_and(|st| st.connector.is_some() && st.resize_overlay.active());
            // Read live from the session's control rather than mirrored into StreamState:
            // the pump knows whether an uplink exists, and a mirrored copy would go stale
            // at session end.
            let mic_muted = stream.as_ref().is_some_and(|st| st.handle.mic.muted());
            // The badge clears itself once a local mute has been read; the mask's own clock
            // lives here because only the frame loop knows when it last moved.
            let audio_mute = stream
                .as_ref()
                .and_then(|st| st.connector.as_ref())
                .and_then(|c| {
                    let mask = c.audio_mute();
                    if mask != audio_mute_seen {
                        audio_mute_seen = mask;
                        audio_mute_at = Instant::now();
                    }
                    punktfunk_core::client::audio_mute_notice(mask, audio_mute_at.elapsed())
                });
            let ring_facts = stream
                .as_ref()
                .filter(|st| st.connector.is_some())
                .map(|st| ring_facts(st, &opts, stats_verbosity, mic_muted, ring_opener));
            let ctx = FrameCtx {
                width: pw,
                height: ph,
                ten_bit: presenter.ten_bit(),
                // Re-read per frame: dragging to a second monitor with a different scale
                // updates this.
                scale: overlay_scale(window.display_scale(), osd_scale_pref),
                stats,
                hint,
                access: access_chip.as_deref(),
                notice: session_notice,
                mic_muted,
                audio_mute,
                resizing,
                pad: pad.as_ref().map(|p| p.name.as_str()),
                pad_pref: pad.as_ref().map(|p| p.pref),
                pads: &pads,
                ring: ring_facts.as_ref(),
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
        overlay_damage.rendered(overlay_frame.as_ref().map(|f| f.image));

        let mut presented_video = false;
        if let Some(st) = &mut stream {
            // Mastering metadata (0xCE) → the presentation engine, ahead of the frame
            // that needs it.
            if let Some(c) = &st.connector {
                while let Ok(m) = c.next_hdr_meta(Duration::ZERO) {
                    presenter.set_hdr_metadata(m);
                }
            }
            // Present-wait completions drive the latch clock, the glass gate, and the
            // host-facing grid — drained every pass (a 1 Hz batch would starve all three).
            if presenter.present_timing_active() {
                let samples = presenter.take_presented_samples();
                if !samples.is_empty() {
                    let clock_offset_ns = st
                        .clock_offset
                        .as_ref()
                        .map_or(0, |o| o.load(Ordering::Relaxed));
                    let period = st.clock.period_ns();
                    let mut stamps = Vec::with_capacity(samples.len());
                    for s in &samples {
                        let e2e = (s.displayed_ns as i128 + clock_offset_ns as i128
                            - s.pts_ns as i128)
                            .max(0) as u64;
                        // Hand the audio plane the figure it has to hit: the on-glass branch.
                        if e2e > 0 && e2e < 10_000_000_000 {
                            if let Some(c) = st.video_e2e.as_ref() {
                                c.store(e2e, Ordering::Relaxed);
                            }
                        }
                        if let Some(c) = &st.connector {
                            c.hud().note_displayed(
                                s.pts_ns,
                                s.decoded_ns,
                                s.submitted_ns,
                                s.displayed_ns,
                            );
                        }
                        // Latch miss: glass later than one panel period past submit plus
                        // the lead we already applied. Store evictions happen whenever
                        // the stream out-runs the panel and say nothing about the latch.
                        if st.store.is_smoothing()
                            && s.displayed_ns.saturating_sub(s.submitted_ns) > period + st.margin_ns
                        {
                            st.win_misses += 1;
                        }
                        if st.last_displayed_ns != 0 && period > 0 {
                            let steps = (s.displayed_ns.saturating_sub(st.last_displayed_ns)
                                + period / 2)
                                / period;
                            st.win_steps[(steps as usize).min(5)] += 1;
                        }
                        st.last_displayed_ns = s.displayed_ns;
                        stamps.push(s.displayed_ns);
                    }
                    st.clock.note_batch(&stamps, st.store.is_smoothing());
                    // VRR probe: healthy-window stamps only. Use the display mode's period
                    // (not the learned one — a slow stream makes the learner adopt our
                    // cadence as "the grid"). FIFO-family only: MAILBOX/IMMEDIATE never
                    // wait for vblank, so they would look like VRR. Else Unknown.
                    let healthy = st.last_forced == 0;
                    if presenter.vblank_locked() {
                        st.cadence.note(&stamps, st.mode_period_ns, healthy);
                    }
                    // Phase-locked capture, the presenter's half: publish the grid the
                    // local clock just learned, so the report and the scheduler cannot
                    // disagree.
                    if let Some(grid) = &st.latch_grid {
                        grid.period_ns
                            .store(st.clock.period_ns(), Ordering::Relaxed);
                        grid.anchor_ns
                            .store(st.clock.anchor_ns(), Ordering::Relaxed);
                    }
                }
            }

            // Intake into the intent store. PyroWave collapses smoothness to latency:
            // its plane-ring retirement assumes the newest-wins hand-off, and all-intra
            // frames make buffering moot.
            while let Ok(f) = st.frames.try_recv() {
                #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
                if st.store.is_smoothing() && matches!(f.image, DecodedImage::PyroWave(_)) {
                    st.store.force_latency();
                    if !st.pyro_latency_forced {
                        st.pyro_latency_forced = true;
                        tracing::info!(
                            "PyroWave stream — smoothness buffering does not apply \
                             (latency pacing)"
                        );
                    }
                }
                // Intent after any PyroWave collapse above, so a wavelet stream folds
                // nothing into a loop it will never consult.
                let smoothing = st.store.is_smoothing();
                let due_ns = st
                    .pacer
                    .due_ns(smoothing, f.pts_ns, f.decoded_ns, st.source_interval_ns)
                    .unwrap_or(0);
                st.store.submit(Paced { frame: f, due_ns });
            }

            // One frame out: latency takes the newest whenever the glass gate allows;
            // smoothness serves the frame whose due time has come.
            let now_ns = session::now_ns();
            st.pacer.follow(st.cadence.verdict());
            let mut to_present = if st.store.is_smoothing() {
                if st.pacer.free_running() {
                    // Variable refresh, measured: the panel refreshes when we present, so
                    // there is no grid to aim at and the due time is the target.
                    st.store.take(|p| p.due_ns <= now_ns as i64)
                } else {
                    // The first latch still reachable from here, given the submit lead. A
                    // frame due before it cannot be shown sooner by waiting; one due after
                    // it would land a slot early (`next_slot_after` is monotone).
                    let slot = st
                        .clock
                        .next_slot_after(now_ns.saturating_add(st.margin_ns));
                    // One present per slot. Two frames due before the same slot were
                    // presented back to back, and MAILBOX showed one of them for nothing
                    // while the next slot went empty: the 0/2-step pairs in the ledger.
                    if slot == st.last_slot_ns {
                        None
                    } else {
                        let taken = st.store.take(|p| p.due_ns < slot as i64);
                        if taken.is_some() {
                            st.last_slot_ns = slot;
                        }
                        taken
                    }
                }
            } else {
                st.store.take(|_| true)
            };
            // FIFO glass budget: one undisplayed present in flight, so the swapchain's
            // own FIFO can never become a standing queue. Only FIFO modes queue and only
            // present timing can count; everywhere else this stays inert.
            if pacing_active && presenter.needs_glass_gate() && presenter.present_timing_active() {
                if let Some(f) = to_present.take() {
                    if st.gate.open(presenter.presents_outstanding(), now_ns) {
                        to_present = Some(f);
                    } else {
                        // Parked: a newest-wins store replaces it if a fresher frame
                        // lands; the waiter's wake (or the 100 ms stale force-open) retries.
                        st.store.put_back(f);
                    }
                }
            }
            st.busy_retry = false;
            if let Some(Paced { frame: f, due_ns }) = to_present {
                // Resize end: a frame at the steered target size means the new-mode
                // picture is here.
                let (fw, fh) = f.image.dimensions();
                st.resize_overlay.decoded(fw, fh);
                st.last_video = Some((fw, fh));
                let DecodedFrame {
                    pts_ns,
                    decoded_ns,
                    image,
                } = f;
                let did_present = match image {
                    // PyroWave: already on the presenter's device and fence-complete — a
                    // present failure has no demote rung; only device loss ends the session.
                    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
                    DecodedImage::PyroWave(f) => {
                        // Wavelet stream carries negotiated ColorInfo (no VUI): a PQ
                        // session presents through the HDR10 path like the H.26x codecs.
                        st.hdr = f.color.is_pq();
                        st.hdr_untonemapped = false;
                        match presenter.present(
                            &window,
                            FrameInput::PyroWave(f),
                            overlay_frame.as_ref(),
                        ) {
                            Ok(Presented::Shown) => {
                                st.pyro_present_warned = false;
                                true
                            }
                            Ok(Presented::Stale) => false,
                            Ok(Presented::Busy(input, on)) => {
                                st.hold_busy(on, input.into_image(), pts_ns, decoded_ns, due_ns);
                                false
                            }
                            Err(e) => {
                                if device_lost(&e) {
                                    return Err(e).context("GPU device lost");
                                }
                                if !st.pyro_present_warned {
                                    st.pyro_present_warned = true;
                                    tracing::warn!(
                                        error = %format!("{e:#}"),
                                        "pyrowave present failed — suppressing repeats until it recovers"
                                    );
                                }
                                false
                            }
                        }
                    }
                    DecodedImage::Cpu(c) => {
                        st.hdr = c.color.is_pq();
                        // Software lane uploads planes into the same planar CSC pass as
                        // hardware, so PQ is tone-mapped there too.
                        st.hdr_untonemapped = false;
                        // Last rung: a present failure has nothing left to demote to.
                        // Drop the frame and keep the session; only a lost device ends it.
                        // The borrow of `c` ends inside `map`, so a busy frame can go back whole.
                        let outcome = presenter
                            .present(&window, FrameInput::Cpu(&c), overlay_frame.as_ref())
                            .map(|p| match p {
                                Presented::Shown => Ok(true),
                                Presented::Stale => Ok(false),
                                Presented::Busy(_, on) => Err(on),
                            });
                        match outcome {
                            Ok(Ok(shown)) => {
                                st.cpu_present_warned = false;
                                shown
                            }
                            Ok(Err(on)) => {
                                st.hold_busy(
                                    on,
                                    Some(DecodedImage::Cpu(c)),
                                    pts_ns,
                                    decoded_ns,
                                    due_ns,
                                );
                                false
                            }
                            Err(e) => {
                                if device_lost(&e) {
                                    return Err(e).context("GPU device lost");
                                }
                                if !st.cpu_present_warned {
                                    st.cpu_present_warned = true;
                                    tracing::warn!(
                                        error = %format!("{e:#}"),
                                        "software present failed — suppressing repeats until it recovers"
                                    );
                                }
                                false
                            }
                        }
                    }
                    // VAAPI output: dmabuf fds plus a plane layout. Import and failure-
                    // streak demotion are the same contract as the other hardware arms.
                    #[cfg(target_os = "linux")]
                    DecodedImage::NativeDmabuf(d)
                        if presenter.supports_dmabuf() && !st.dmabuf_demoted =>
                    {
                        st.hdr = d.color.is_pq();
                        st.hdr_untonemapped = false;
                        match presenter.present(
                            &window,
                            FrameInput::Dmabuf(d),
                            overlay_frame.as_ref(),
                        ) {
                            Ok(Presented::Shown) => {
                                st.hw_fails = 0;
                                true
                            }
                            Ok(Presented::Stale) => false,
                            Ok(Presented::Busy(input, on)) => {
                                st.hold_busy(on, input.into_image(), pts_ns, decoded_ns, due_ns);
                                false
                            }
                            // Import/CSC failure is survivable — a streak means this box
                            // cannot do the hw path: demote the decoder to software. A lost
                            // device is not survivable and must not demote.
                            Err(e) => {
                                if device_lost(&e) {
                                    return Err(e).context("GPU device lost");
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
                    DecodedImage::NativeDmabuf(_) => {
                        // No import extensions (or already demoted) — the pump rebuilds
                        // the decoder as software.
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
                    // D3D11VA: shared-texture import, same gate + failure-streak demotion
                    // as dmabuf.
                    #[cfg(windows)]
                    DecodedImage::D3d11(d) if presenter.supports_d3d11() && !st.dmabuf_demoted => {
                        st.hdr = d.color.is_pq();
                        st.hdr_untonemapped = false;
                        match presenter.present(
                            &window,
                            FrameInput::D3d11(d),
                            overlay_frame.as_ref(),
                        ) {
                            Ok(Presented::Shown) => {
                                st.hw_fails = 0;
                                true
                            }
                            Ok(Presented::Stale) => false,
                            Ok(Presented::Busy(input, on)) => {
                                st.hold_busy(on, input.into_image(), pts_ns, decoded_ns, due_ns);
                                false
                            }
                            Err(e) => {
                                if device_lost(&e) {
                                    return Err(e).context("GPU device lost");
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
                        // No import extensions (or already demoted) — the pump rebuilds
                        // the decoder as software.
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
                    // Native Vulkan Video: decoded on the presenter's own device —
                    // present is views + CSC, no import step. Same failure-streak demotion.
                    // A drained/demoted frame drops through the arm below — its guard
                    // still returns the decoder's slot.
                    DecodedImage::NativeVk(v) if !st.dmabuf_demoted => {
                        st.hdr = v.color.is_pq();
                        st.hdr_untonemapped = false;
                        match presenter.present(
                            &window,
                            FrameInput::NativeVk(v),
                            overlay_frame.as_ref(),
                        ) {
                            Ok(Presented::Shown) => {
                                st.hw_fails = 0;
                                true
                            }
                            Ok(Presented::Stale) => false,
                            Ok(Presented::Busy(input, on)) => {
                                st.hold_busy(on, input.into_image(), pts_ns, decoded_ns, due_ns);
                                false
                            }
                            Err(e) => {
                                if device_lost(&e) {
                                    return Err(e).context("GPU device lost");
                                }
                                st.hw_fails += 1;
                                tracing::warn!(error = %format!("{e:#}"), fails = st.hw_fails,
                                    "native vulkan present failed");
                                if st.hw_fails >= 3 {
                                    st.dmabuf_demoted = true;
                                    tracing::warn!("demoting the decoder to software");
                                    st.force_software.store(true, Ordering::Relaxed);
                                }
                                false
                            }
                        }
                    }
                    DecodedImage::NativeVk(_) => false, // demoted — drain until rebuild
                };
                if did_present {
                    presented_video = true;
                    overlay_damage.video_presented(Instant::now());
                    let (import_us, submit_us) = presenter.last_timings();
                    st.win_import_us.push(import_us);
                    st.win_submit_us.push(submit_us);
                    let (fence_us, acquire_us, present_us) = presenter.last_waits();
                    st.win_fence_us.push(fence_us);
                    st.win_acquire_us.push(acquire_us);
                    st.win_present_us.push(present_us);
                    if opts.json_status && !st.ready_announced {
                        st.ready_announced = true;
                        emit(SessionLine::Ready);
                    }
                    if presenter.present_timing_active() {
                        // Hand the frame's stamps to the present-wait waiter — e2e/display
                        // samples arrive via `take_presented_samples` with a true on-glass stamp.
                        presenter.note_presented(pts_ns, decoded_ns);
                        st.gate.note_present(now_ns);
                        st.win_out_max = st.win_out_max.max(presenter.presents_outstanding());
                    } else {
                        let displayed_ns = session::now_ns();
                        let clock_offset_ns = st
                            .clock_offset
                            .as_ref()
                            .map_or(0, |o| o.load(Ordering::Relaxed));
                        let e2e = (displayed_ns as i128 + clock_offset_ns as i128 - pts_ns as i128)
                            .max(0) as u64;
                        // Same hand-off as the glass-stamped branch. Anchored on submit, so it
                        // understates the video leg by up to a refresh: inside the audio deadband.
                        if e2e > 0 && e2e < 10_000_000_000 {
                            if let Some(c) = st.video_e2e.as_ref() {
                                c.store(e2e, Ordering::Relaxed);
                            }
                        }
                        if let Some(c) = &st.connector {
                            c.hud().note_displayed(pts_ns, decoded_ns, 0, displayed_ns);
                        }
                        // No glass stamps: the submit instant anchors an approximate grid
                        // on the mode's refresh period, so smoothness still drains one
                        // frame per (approximate) slot.
                        st.clock
                            .note_batch(&[displayed_ns], st.store.is_smoothing());
                    }
                }
            }

            // Close the overlay window once per second.
            if st.win_start.elapsed() >= Duration::from_secs(1) {
                let import = punktfunk_core::hud::Summary::of(&mut st.win_import_us);
                let submit = punktfunk_core::hud::Summary::of(&mut st.win_submit_us);
                let fence = punktfunk_core::hud::Summary::of(&mut st.win_fence_us);
                let acquire = punktfunk_core::hud::Summary::of(&mut st.win_acquire_us);
                let queue_present = punktfunk_core::hud::Summary::of(&mut st.win_present_us);
                // Drained once per window and shared by the HUD and the log line — a
                // second `take_counters` would read zeros.
                let (replaced, q_drop, q_dry) = st.store.take_counters();
                let (gated, forced) = st.gate.take_counters();
                st.last_forced = forced;
                let present = PresentCounters {
                    mode: presenter.present_mode_name(),
                    vrr: st.cadence.verdict(),
                    smoothing: st.store.is_smoothing(),
                    q_drop,
                    q_dry,
                    gated,
                    forced,
                };
                st.win_import_us.clear();
                st.win_submit_us.clear();
                st.win_fence_us.clear();
                st.win_acquire_us.clear();
                st.win_present_us.clear();
                let (pace_ms, latch_ms) =
                    close_window(st, &presenter, &present, replaced, stats_verbosity);
                st.win_start = Instant::now();
                // Adaptive slot margin: start at 0 — a fixed lead is display tax — and
                // widen one step per window whose measured latch misses demand it.
                // One-way per stream.
                if st.store.is_smoothing() && st.win_misses > 2 && st.margin_ns < MARGIN_MAX_NS {
                    st.margin_ns = (st.margin_ns + MARGIN_STEP_NS).min(MARGIN_MAX_NS);
                    tracing::info!(
                        margin_us = st.margin_ns / 1000,
                        misses = st.win_misses,
                        "smoothness slot margin widened (measured latch misses)"
                    );
                }
                // The 1 Hz presenter line, always: the field bundle's only record of where a
                // frame went after decode and how evenly the glass stepped.
                if pacing_active {
                    let _ = present_debug;
                    let cadence_health = st.pacer.health();
                    let shown: u32 = st.win_steps.iter().sum();
                    let mode_count = st.win_steps.iter().copied().max().unwrap_or(0);
                    // Spacings off the most common step, per mille of the window's presents.
                    let judder = if shown > 0 {
                        u64::from(shown - mode_count) * 1000 / u64::from(shown)
                    } else {
                        0
                    };
                    tracing::info!(
                        smoothing = present.smoothing,
                        mode = present.mode,
                        vrr = present.vrr.label(),
                        replaced,
                        q_drop,
                        q_dry,
                        gated,
                        forced,
                        misses = st.win_misses,
                        out_max = st.win_out_max,
                        steps = ?st.win_steps,
                        busy = ?st.win_busy,
                        judder,
                        pace_ms,
                        latch_ms,
                        import_us = import.p50_us,
                        submit_us = submit.p50_us,
                        submit_max_us = submit.max_us,
                        fence_us = fence.p50_us,
                        fence_max_us = fence.max_us,
                        acquire_us = acquire.p50_us,
                        acquire_max_us = acquire.max_us,
                        present_us = queue_present.p50_us,
                        period_us = st.clock.period_ns() / 1000,
                        margin_us = st.margin_ns / 1000,
                        // Cadence loop's current hold and the jitter it is sized from,
                        // plus frames whose due time had already passed when they arrived.
                        // Cumulative/instantaneous, not window sums like the counters above.
                        cushion_us = cadence_health.cushion_ns / 1000,
                        jitter_us = cadence_health.jitter_ns / 1000,
                        late = cadence_health.late,
                        "presenter window"
                    );
                }
                st.win_misses = 0;
                st.win_out_max = 0;
                st.win_steps = [0; 6];
                st.win_busy = [0; 2];
            }
        }

        // Present the overlay alone when no video frame carried it: every pass across a
        // resize scrim (the host's rebuild gap), and once per change while browsing or
        // after a mid-stream picture has gone still. An idle console hands back the same
        // image, so browsing presents only what the overlay re-rendered.
        let resize_scrim = stream.as_ref().is_some_and(|s| s.resize_overlay.active());
        let browse_idle = matches!(mode, ModeCtl::Browse(_))
            && stream.as_ref().is_none_or(|s| s.connector.is_none());
        let still_picture = stream.as_ref().is_some_and(|s| s.last_video.is_some())
            && overlay_damage.take_due(Instant::now());
        let browse_changed = browse_idle && overlay_damage.take_dirty();
        if !presented_video && (resize_scrim || browse_changed || still_picture) {
            // The UI owns the screen: hand the swapchain back to SDR. A finished PQ stream
            // leaves HDR10 live, and UI presents carry no frame. Not applied to
            // `resize_scrim`: that gap is still an HDR session, and flipping would rebuild
            // the swapchain twice.
            if browse_idle {
                presenter.leave_hdr(&window)?;
            }
            presenter.present(&window, FrameInput::Redraw, overlay_frame.as_ref())?;
        }
    };

    // Every loop exit converges here, so gamepad teardown belongs here, not on the
    // individual breaks. `detach` only queues; the close (flush, GamepadRemove, rumble
    // stop) runs when the pump drains it. Breaking immediately after detach leaves
    // pads unflushed and, if rumbling, still buzzing.
    pump.shutdown();
    // Join the pump before the device-wide idle: its decode submissions would race
    // vkDeviceWaitIdle otherwise.
    if let Some(st) = stream.take() {
        st.shutdown();
    }
    // Overlay resources live on the presenter's device: quiesce the queue first, drop
    // the overlay, then the presenter tears down.
    presenter.wait_idle();
    drop(overlay);
    Ok(outcome)
}

/// An `SDL_DisplayMode` as the panel's real pixels — the `0 = native` stream mode.
///
/// SDL3 reports a display mode in screen coordinates and hands the ratio separately as
/// `pixel_density`. On X11 and Windows that ratio is 1.0, so this is a no-op. Under
/// Wayland fractional scaling, taking `m.w`/`m.h` raw negotiates the point size and
/// streams a blurry image. The density is the exact `pixels / points` ratio, so the
/// multiplication recovers the panel size to the pixel.
fn native_mode(w: i32, h: i32, pixel_density: f32, refresh_rate: f32) -> Mode {
    // A non-finite or non-positive density is SDL telling us nothing useful; 1×
    // preserves the reported size instead of collapsing the mode to zero.
    let density = if pixel_density.is_finite() && pixel_density > 0.0 {
        pixel_density
    } else {
        1.0
    };
    let px = |v: i32| (v.max(0) as f32 * density).round().max(0.0) as u32;
    Mode {
        width: px(w),
        height: px(h),
        refresh_hz: refresh_rate.round().max(0.0) as u32,
    }
}

/// The HDR volume of the display the window is on now, read per launch, so a console
/// moved to a TV asks for the TV's HDR. `None` for an SDR display, and off Windows.
fn window_display_hdr(window: &sdl3::video::Window) -> Option<HdrMeta> {
    #[cfg(windows)]
    return pf_client_core::video_d3d11::display_hdr_volume(crate::win32::window_monitor(window));
    #[cfg(not(windows))]
    {
        let _ = window;
        None
    }
}

/// Inside a gamescope session? `overlay_focus` exists only on Linux; elsewhere, no.
fn in_gamescope() -> bool {
    #[cfg(target_os = "linux")]
    {
        pf_client_core::overlay_focus::gamescope_session()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// KDE fractional scaling advertises points; "Native" must recover the panel pixels.
    #[test]
    fn native_is_the_panels_pixels_under_fractional_wayland_scaling() {
        let density = 2560.0 / 1707.0;
        let m = native_mode(1707, 1067, density, 165.0);
        assert_eq!((m.width, m.height, m.refresh_hz), (2560, 1600, 165));
        // Survives the even-floor `validate_dimensions` forces (1707×1067 lost its odd
        // pixel and became 1706×1066).
        assert_eq!(
            punktfunk_core::render_scale::apply(m.width, m.height, 1.0, 8192),
            (2560, 1600)
        );
        assert_eq!(
            punktfunk_core::render_scale::apply(1707, 1067, 1.0, 8192),
            (1706, 1066),
            "the pre-fix mode, kept here so the regression is legible"
        );
    }

    #[test]
    fn native_is_unchanged_where_the_density_is_one() {
        // X11, Windows, and Wayland at 100 % all report 1.0 — density 1.0 is a no-op.
        let m = native_mode(2560, 1600, 1.0, 165.0);
        assert_eq!((m.width, m.height, m.refresh_hz), (2560, 1600, 165));
        // Integer scaling (a 200 % 4K panel reported as 1920×1080 points) doubles cleanly.
        let m = native_mode(1920, 1080, 2.0, 60.0);
        assert_eq!((m.width, m.height), (3840, 2160));
    }

    #[test]
    fn a_nonsense_density_falls_back_to_one_rather_than_zeroing_the_mode() {
        // SDL normalizes an unset density to 1.0, but this must not be the one place a
        // driver quirk can hand the host a 0×0 mode request.
        for bogus in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let m = native_mode(2560, 1600, bogus, 60.0);
            assert_eq!((m.width, m.height), (2560, 1600), "density {bogus}");
        }
        // A negative mode size is clamped, not wrapped into a huge u32.
        let m = native_mode(-1, -1, 1.5, 60.0);
        assert_eq!((m.width, m.height), (0, 0));
    }
}
