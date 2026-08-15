//! The presenter↔console-UI contract (punktfunk-planning
//! `linux-client-rearchitecture.md` §6.1): the presenter exposes its device and
//! composites at most ONE sampled RGBA quad per frame; the overlay implementation
//! (pf-console-ui, Skia) fills offscreen images on its own damage-driven schedule. No
//! Skia type crosses this line — everything here is ash — and a `frame()` returning
//! `None` costs the hot path nothing (the quad isn't even recorded).

use ash::vk;
use pf_client_core::gamepad::{MenuEvent, MenuPulse};
use punktfunk_core::config::GamepadPref;

/// The presenter's device, shared with the overlay so its renderer (Skia's
/// `DirectContext`) creates resources on the same VkDevice/queue. Handles stay valid for
/// the presenter's lifetime — the overlay must be dropped before it (the run loop owns
/// both and drops the overlay first).
pub struct SharedDevice {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue: vk::Queue,
    pub queue_family_index: u32,
    /// External-sync lock for `queue` — the decode lane submits to the same queue
    /// from the pump thread, so every overlay flush/submit must hold it. The presenter,
    /// this overlay and the native decode lane all serialize on this one lock; take it
    /// with [`pf_client_core::video::QueueLock::guard`], whose RAII form is what every
    /// Rust caller wants.
    pub queue_lock: std::sync::Arc<pf_client_core::video::QueueLock>,
    /// The Vulkan version an overlay renderer may size its function table to — the lower of
    /// [`crate::vk::INSTANCE_API_VERSION`] (what `VkApplicationInfo::apiVersion` declared for
    /// `instance`) and what the loader provides.
    ///
    /// **Cap yourself here; do not ask the loader yourself.** Entry points above this version
    /// were never promised to us — `vkGetDeviceProcAddr` returns null for them — so a renderer
    /// that probes `vkEnumerateInstanceVersion` instead (a current Mesa answers 1.4 where we
    /// asked for 1.3) validates a function table it can never fill and refuses to start. That
    /// is exactly how the Skia console UI died in 0.28.0; see the note in `pf-console-ui`'s
    /// `SkiaOverlay::init`.
    pub api_version: u32,
}

/// What the overlay may draw this frame — composed by the run loop from session state.
/// Milestone 1 (OSD/HUD) is text-shaped; the console library replaces this with a
/// richer scene enum when it moves in.
pub struct FrameCtx<'a> {
    /// Swapchain size in pixels — the overlay renders 1:1.
    pub width: u32,
    pub height: u32,
    /// UI scale for the stream chrome: the window's display scale (DPI × the display's content
    /// scale — `1.0` at 96 dpi / 100 %), times the `PUNKTFUNK_OSD_SCALE` preference. Because the
    /// overlay renders in *physical* pixels, a fixed-pixel OSD shrinks as panel density rises —
    /// unreadable at 14 px on a 4K laptop at 200 %. Every chrome metric is multiplied by this.
    /// Sanitized and clamped by the run loop (`overlay_scale`), so it is always finite and > 0.
    pub scale: f32,
    /// Multi-line stats OSD (top-left panel); `None` = hidden.
    pub stats: Option<&'a str>,
    /// The capture hint (bottom-center pill, "click to capture…"); `None` = hidden.
    pub hint: Option<&'a str>,
    /// The access chip (per-client access §7 "say what this session is"): a small standing
    /// pill — "Controller only · ends in 1 h 58 m" — drawn at every stats tier, `None` for
    /// a full-control permanent session (today's default look, and every old host).
    pub access: Option<&'a str>,
    /// A transient access toast ("Access is now Controller only", "Access ends in 5 m") —
    /// takes the hint pill's slot with priority while up. The run loop owns its timing.
    pub notice: Option<&'a str>,
    /// The user muted their microphone mid-stream (Ctrl+Alt+Shift+V). Draws a persistent
    /// badge, deliberately independent of the stats tier: a muted mic is a fact about what
    /// the host is hearing, and "did my mute take?" must be answerable with the overlay off.
    /// False whenever this session has no mic uplink at all — the badge never invents one.
    pub mic_muted: bool,
    /// A mid-stream Match-window resize is in flight (design/midstream-resolution-resize.md,
    /// client UX): draw a full-screen scrim + spinner so the host's 0.3–2 s virtual-display
    /// and encoder rebuild reads as an intentional pause rather than the stream stretching to
    /// the changed window. Cleared the instant the sharp new-resolution frame is on glass.
    pub resizing: bool,
    /// The active gamepad's name (the console library's controller chip).
    pub pad: Option<&'a str>,
    /// The active pad's resolved kind — drives the console UI's button glyphs
    /// (PlayStation shapes for DualSense/DualShock, ABXY letters otherwise).
    pub pad_pref: Option<GamepadPref>,
    /// Every connected pad (the console settings' "Use controller" row).
    pub pads: &'a [pf_client_core::gamepad::PadInfo],
}

/// One overlay image ready to composite: RGBA, PREMULTIPLIED alpha, already in
/// `SHADER_READ_ONLY_OPTIMAL`, sized `width`×`height` (normally the `FrameCtx` size; a
/// stale size during a resize just stretches for a frame).
pub struct OverlayFrame {
    pub image: vk::Image,
    pub view: vk::ImageView,
    pub width: u32,
    pub height: u32,
}

/// An action the overlay raises out of its input handling (browse mode). Only actions
/// the RUN LOOP must act on live here — starting/canceling sessions and quitting; data
/// work (pairing, discovery, library fetches…) rides the console command bus instead.
pub enum OverlayAction {
    /// Start a session on this host. `launch` carries a library title id on the Hello
    /// (`None` streams the desktop); `title` is display-only (window title).
    Launch {
        addr: String,
        port: u16,
        fp_hex: String,
        launch: Option<String>,
        title: String,
        /// One-off settings-profile override for THIS launch (a profile id — a pinned
        /// card's connect). `None` resolves the host's default binding as before; the
        /// binary feeds it to `trust::effective_settings`, so a dangling id quietly
        /// falls back to the defaults and never blocks the connect.
        profile: Option<String>,
        /// The no-PIN delegated-approval path: pin the host's advertised fingerprint and
        /// open a connect the host PARKS until the operator approves this device in its
        /// console (a long connect budget), then persist it as paired. `false` = an
        /// ordinary connect to an already-paired host.
        request_access: bool,
    },
    /// Abort an in-flight connect (B while Connecting) — the console keeps browsing.
    /// The run loop stops the pump; a dial that already won the race is quit-closed.
    CancelConnect,
    /// Quit the launcher (B at the root) — ends the process, Gaming Mode returns.
    Quit,
    /// Put this text on the system clipboard (the host menu's "Copy link"). An action
    /// rather than a console command because the clipboard belongs to SDL, which lives on
    /// the run loop's thread and nowhere else.
    CopyText(String),
}

/// Which button a [`PointerInput`] press/release carries. A touchscreen contact always
/// arrives as `Primary` — there is no second finger-button, and the console's back
/// affordance is on glass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Primary,
    /// The right button — the console reads it as Back, the pointer's B.
    Secondary,
}

/// Pointer or touch input offered to the overlay, in SWAPCHAIN PIXELS.
///
/// Pixels, not window coordinates, because that is the space the overlay renders in: a
/// screen hit-tests the very rects it drew last frame instead of re-deriving a layout
/// through the display scale. The run loop owns the conversion — it is the side that
/// holds the window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerInput {
    Move {
        x: f32,
        y: f32,
    },
    Down {
        x: f32,
        y: f32,
        button: PointerButton,
    },
    Up {
        x: f32,
        y: f32,
        button: PointerButton,
    },
    /// One wheel/trackpad scroll step at `x`/`y`; `dy` > 0 scrolls away from the user.
    Wheel {
        x: f32,
        y: f32,
        dy: f32,
    },
    /// The gesture was abandoned (the pointer left the window, the touch was canceled) —
    /// any armed press is dropped without acting.
    Cancel,
}

/// Session lifecycle notifications into the overlay (browse mode drives its scenes off
/// these; the OSD/HUD ignore them).
pub enum SessionPhase<'a> {
    /// A launch action was accepted — the connect is in flight.
    Connecting,
    /// Connected; frames are coming.
    Streaming,
    /// The connect failed (browse mode returns to the library with this message).
    Failed(&'a str),
    /// The session ran and ended (`Some` = abnormal reason for the status strip).
    Ended(Option<&'a str>),
    /// The session ended and the client is DIALING AGAIN by itself — today only because
    /// the negotiated codec ran out of decode rungs (M8's software-HEVC drop) and the
    /// retry advertises a codec this device can actually finish.
    ///
    /// Distinct from [`Self::Ended`] and [`Self::Failed`] because the user's next action
    /// is different: nothing. "Session ended — HEVC decoding failed" invites a manual
    /// reconnect that is already in flight, and "Couldn't connect" is simply false — the
    /// connect worked, the decode did not.
    Reconnecting(&'a str),
}

/// The console-UI side. Object-safe; the session binary passes
/// `Option<Box<dyn Overlay>>` (None = the Skia-free power-user build).
pub trait Overlay {
    /// One-time setup on the presenter's device.
    fn init(&mut self, shared: &SharedDevice) -> anyhow::Result<()>;

    /// Input routing, before capture sees the event. `true` = consumed (the library or
    /// a menu is up) — the event must not reach capture/forwarding.
    fn handle_event(&mut self, event: &sdl3::event::Event) -> bool;

    /// Gamepad menu-mode navigation (browse mode; the run loop drains the service's
    /// menu channel). Returns a haptic pulse to play on the menu pad, if any.
    fn handle_menu(&mut self, _event: MenuEvent) -> Option<MenuPulse> {
        None
    }

    /// Mouse/touch input, in swapchain pixels, before capture sees it. `true` = consumed
    /// (the console is up and something under the pointer took it) — the event must not
    /// reach capture/forwarding.
    ///
    /// Separate from [`Self::handle_event`] because the window→pixel conversion belongs to
    /// the run loop, which is the side that holds the window: the overlay renders in
    /// pixels and would otherwise have to re-derive the display scale it never sees.
    fn handle_pointer(&mut self, _input: PointerInput) -> bool {
        false
    }

    /// Drain one pending action raised by handled input. Called once per loop
    /// iteration; return `None` when idle.
    fn take_action(&mut self) -> Option<OverlayAction> {
        None
    }

    /// A session lifecycle edge (browse mode scene driving).
    fn session_phase(&mut self, _phase: SessionPhase) {}

    /// True while a text field is being edited — the run loop starts/stops SDL text
    /// input to match (IME + `Event::TextInput` delivery on desktop; under gamescope
    /// this is also what lets Steam's on-screen keyboard type into the app).
    fn text_input_active(&self) -> bool {
        false
    }

    /// Once per presenter iteration. Damage-driven: re-render (flush + transition to
    /// SHADER_READ_ONLY) only when the content or size changed, else return the previous
    /// image. `None` = nothing to composite. The returned image must stay untouched
    /// until `frame()` runs again (the presenter runs one frame in flight and the
    /// implementation keeps a ring of two, so alternating satisfies this).
    fn frame(&mut self, ctx: &FrameCtx) -> anyhow::Result<Option<OverlayFrame>>;
}
