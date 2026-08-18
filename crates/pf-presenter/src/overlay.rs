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

// `OverlayAction`, `PointerButton`, `PointerInput` and `SessionPhase` are plain data shared
// by the console shell on every platform it runs on (the Vulkan session here, the Android
// client's GL host), so they live in the portable `pf_client_core::console` module. Re-exported
// under their old paths — `run.rs` and the session's console service spell them
// `pf_presenter::overlay::…`.
pub use pf_client_core::console::{OverlayAction, PointerButton, PointerInput, SessionPhase};

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
